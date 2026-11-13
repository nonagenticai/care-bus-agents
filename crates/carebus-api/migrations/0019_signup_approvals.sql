-- care-bus 17: early-access signup (early-access v2, contracts C1-C6). Ported from agent 0030 + 0040 (the CP shape:
-- locale, source web|chat|invite, the approve claim) for care-bus: tenancy is `tenants` + `users`, so there is no
-- member_invites / member_entitlement here. Forward-only (pre-prod).
--
-- 1. `signup_approvals`: the queue. Platform data: no tenant_id and no RLS (a pending signup has no tenant yet).
--    Every route that reaches it is guarded in code (intake service account, decider service account, super_admin);
--    carebus_app may read, insert and update it, never delete or truncate it.
-- 2. `signup_provisioning_steps`: what an approval did, one row per (signup, step), with the per-step lease the
--    approve request and the retry tick claim. The set-password email is the last step.
-- 3. `member_licences`: the member licence each care-bus login holds (licence on login, approval). Tenant data,
--    under RLS: written and read only inside the member's tenant context (lesson #1).
-- 4. `carebus_auth_user` also answers whether the member licence is recorded, so the per-request auth read tells
--    the API when to ensure one (one licences call per user, not per request).

CREATE TABLE signup_approvals (
    id                 uuid        PRIMARY KEY DEFAULT gen_random_uuid(),
    email              text        NOT NULL CHECK (email <> '' AND char_length(email) <= 254),
    -- Requester-typed, normalised to one printable line by the route.
    name               text        NOT NULL CHECK (char_length(name) BETWEEN 1 AND 160),
    company            text        CHECK (company IS NULL OR char_length(company) BETWEEN 1 AND 160),
    -- Free text with line breaks kept; never logged. Telegram shows a <=300-char one-line excerpt.
    message            text        CHECK (message IS NULL OR char_length(message) <= 2000),
    -- `invite`: a super_admin's pre-approved invite (no intake behind it).
    source             text        NOT NULL CHECK (source IN ('web', 'chat', 'invite')),
    -- Contract C6; NULL = not told (the email uses en).
    locale             text        CHECK (locale IS NULL OR locale IN ('ar', 'de', 'en', 'es', 'fr', 'he', 'nl', 'pl')),
    trust              text        CHECK (trust IS NULL OR trust IN ('intake')),
    -- The intake client (`azp`) that submitted it; NULL for an invite.
    intake_client      text        CHECK (intake_client IS NULL OR intake_client <> ''),
    -- The super_admin who sent an invite; NULL for an intake row.
    invited_by         text        CHECK (invited_by IS NULL OR btrim(invited_by) <> ''),
    status             text        NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'approved', 'rejected')),
    approved_tenant_id uuid        REFERENCES tenants (id) ON DELETE SET NULL,
    created_at         timestamptz NOT NULL DEFAULT now(),
    decided_at         timestamptz,
    -- The super_admin's `sub`, or `telegram:<user id>` for a tap relayed by chat-bridge.
    decided_by         text,
    decided_via        text        CHECK (decided_via IS NULL OR decided_via IN ('ui', 'telegram')),
    reason             text        CHECK (reason IS NULL OR char_length(reason) <= 500),
    -- The approve claim (CP 0121): ONE approver at a time across replicas, Telegram and the screen.
    approving_until    timestamptz,
    approving_claim    uuid,
    -- Reject repair (CP signup-reject-repair): what a reject undid of an aborted approve, retried by the tick while
    -- `repair_next_at` is set. Codes only, never personal data.
    reject_repair      jsonb       CHECK (reject_repair IS NULL OR jsonb_typeof(reject_repair) = 'object'),
    repair_attempts    integer     NOT NULL DEFAULT 0 CHECK (repair_attempts >= 0),
    repair_next_at     timestamptz,
    CHECK ((status = 'pending') = (decided_at IS NULL)),
    CHECK ((status = 'pending') = (decided_via IS NULL)),
    CHECK (status = 'approved' OR approved_tenant_id IS NULL),
    CHECK ((approving_until IS NULL) = (approving_claim IS NULL)),
    CHECK (status = 'pending' OR approving_claim IS NULL),
    CHECK (status = 'rejected' OR (reject_repair IS NULL AND repair_next_at IS NULL)),
    CONSTRAINT signup_approvals_origin_check CHECK (
        CASE WHEN source = 'invite'
             THEN trust IS NULL AND intake_client IS NULL AND invited_by IS NOT NULL
             ELSE trust IS NOT NULL AND intake_client IS NOT NULL AND invited_by IS NULL END)
);

-- One PENDING row per address (case-insensitive): a re-submit is a no-op while it waits.
CREATE UNIQUE INDEX signup_approvals_pending_email ON signup_approvals (lower(email)) WHERE status = 'pending';
CREATE INDEX signup_approvals_status_created_idx ON signup_approvals (status, created_at DESC, id DESC);
CREATE INDEX signup_approvals_created_idx ON signup_approvals (created_at DESC, id DESC);
CREATE INDEX signup_approvals_repair_due ON signup_approvals (repair_next_at) WHERE repair_next_at IS NOT NULL;

CREATE TABLE signup_provisioning_steps (
    signup_id       uuid        NOT NULL REFERENCES signup_approvals (id) ON DELETE CASCADE,
    step            text        NOT NULL CHECK (step IN ('keycloak_user', 'tenant', 'membership',
                                                         'licences_tenant', 'member_licence', 'invite_email')),
    ord             smallint    NOT NULL CHECK (ord BETWEEN 1 AND 6),
    status          text        NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'ok', 'failed', 'skipped')),
    attempts        integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Machine code of the last failure or skip reason; NULL when ok.
    code            text,
    -- Short human detail of the last failure (no personal data, no secrets).
    message         text        CHECK (message IS NULL OR char_length(message) <= 500),
    -- What a later attempt needs: the Keycloak sub, the planned tenant id, the licence id. Never a token or an email.
    data            jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(data) = 'object'),
    last_attempt_at timestamptz,
    next_attempt_at timestamptz,
    -- The per-step claim; a step is run only by whoever set this to the future.
    lease_until     timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (signup_id, step),
    UNIQUE (signup_id, ord)
);
CREATE INDEX signup_provisioning_steps_due ON signup_provisioning_steps (next_attempt_at) WHERE status = 'pending';

-- Platform tables: the API reads and writes them, never deletes them (0007's default privileges granted DELETE).
REVOKE DELETE, TRUNCATE ON signup_approvals, signup_provisioning_steps FROM carebus_app;
GRANT SELECT, INSERT, UPDATE ON signup_approvals, signup_provisioning_steps TO carebus_app;

-- The member licence of a care-bus login (POST /licenses/user/{sub} on its tenant). One row per login: the API
-- ensures it once, then the auth read says it is recorded. Deleted with the users row.
CREATE TABLE member_licences (
    sub        text        PRIMARY KEY REFERENCES users (sub) ON DELETE CASCADE,
    tenant_id  uuid        NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    license_id text        NOT NULL CHECK (license_id <> '' AND char_length(license_id) <= 64),
    ensured_at timestamptz NOT NULL DEFAULT now()
);
CREATE INDEX member_licences_tenant ON member_licences (tenant_id);
ALTER TABLE member_licences ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON member_licences FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
GRANT SELECT, INSERT, UPDATE, DELETE ON member_licences TO carebus_app;

-- The per-request auth read, now with `member_licence` (whether the login's licence is recorded). A new return type
-- needs DROP + CREATE; the grant is restored below.
DROP FUNCTION carebus_auth_user(text);
CREATE FUNCTION carebus_auth_user(p_sub text)
    RETURNS TABLE (tenant_id uuid, tenant_role text, tenant_status text, member_licence boolean)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT u.tenant_id, u.tenant_role, t.status,
               EXISTS (SELECT 1 FROM member_licences m WHERE m.sub = u.sub AND m.tenant_id = u.tenant_id)
          FROM users u JOIN tenants t ON t.id = u.tenant_id
         WHERE u.sub = p_sub
    $$;
REVOKE ALL ON FUNCTION carebus_auth_user(text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_auth_user(text) TO carebus_app;
