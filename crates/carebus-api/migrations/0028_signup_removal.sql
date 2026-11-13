-- care-bus 17: early-access REMOVAL (offboarding an APPROVED signup), after Studio w2v_orchestrator 0100 and
-- CommonPractice #477. Until now an approved early-access workspace could only be removed by hand (tenant suspended,
-- licences revoked through the licences SA, member deleted, licences tenant archived, Keycloak login deleted), and its
-- signup row stayed `approved`. A super_admin now removes it with one guarded action
-- (crate::signup::removal; `POST /api/signup-approvals/{id}/remove`, confirm = the workspace slug).
--
-- 1. `signup_approvals.status` gains `removed`. A removed row keeps `approved_tenant_id` (the workspace the removal
--    works on) and always carries `removal_state`: `pending` (running / retried by the tick), `blocked` (a guard
--    refused: an operator looks, then Retry), `failed` (a step was refused or ran out of attempts: Retry), `done`.
--    Who asked and when are kept next to it; `removal_next_at` is when the tick runs it next, `removal_lease_until`
--    the one-runner-at-a-time claim (the request or the tick).
-- 2. `signup_removal_steps`: what the removal did, one row per (signup, step), like `signup_provisioning_steps`.
--    Platform data: no tenant_id and no RLS; carebus_app reads, inserts and updates it, never deletes it.

ALTER TABLE signup_approvals DROP CONSTRAINT signup_approvals_status_check;
ALTER TABLE signup_approvals ADD CONSTRAINT signup_approvals_status_check
    CHECK (status IN ('pending', 'approved', 'rejected', 'removed'));

-- 0019's unnamed `CHECK (status = 'approved' OR approved_tenant_id IS NULL)`: found by its definition, so the
-- generated name does not matter. A removed row keeps its workspace id.
DO $$
DECLARE
    v_name text;
BEGIN
    SELECT c.conname INTO v_name
      FROM pg_constraint c
     WHERE c.conrelid = 'signup_approvals'::regclass AND c.contype = 'c'
       AND pg_get_constraintdef(c.oid) ILIKE '%approved_tenant_id IS NULL%'
       AND pg_get_constraintdef(c.oid) NOT ILIKE '%removed%';
    IF v_name IS NULL THEN
        RAISE EXCEPTION '0028: the approved_tenant_id CHECK of 0019 was not found';
    END IF;
    EXECUTE format('ALTER TABLE signup_approvals DROP CONSTRAINT %I', v_name);
END $$;
ALTER TABLE signup_approvals ADD CONSTRAINT signup_approvals_tenant_only_when_approved
    CHECK (status IN ('approved', 'removed') OR approved_tenant_id IS NULL);

ALTER TABLE signup_approvals
    ADD COLUMN removal_state        text CHECK (removal_state IS NULL
                                                OR removal_state IN ('pending', 'blocked', 'failed', 'done')),
    ADD COLUMN removal_requested_by text CHECK (removal_requested_by IS NULL OR btrim(removal_requested_by) <> ''),
    ADD COLUMN removal_requested_at timestamptz,
    ADD COLUMN removal_next_at      timestamptz,
    ADD COLUMN removal_lease_until  timestamptz,
    -- The runner holding the lease (a fresh token per run): renewals and the release only touch their own lease.
    ADD COLUMN removal_lease_token  uuid,
    ADD COLUMN removal_done_at      timestamptz,
    ADD CONSTRAINT signup_approvals_removal_check CHECK (
        (status = 'removed') = (removal_state IS NOT NULL)
        AND (status = 'removed') = (removal_requested_by IS NOT NULL)
        AND (status = 'removed') = (removal_requested_at IS NOT NULL)
        AND ((removal_state = 'done') = (removal_done_at IS NOT NULL) OR removal_state IS NULL)),
    ADD CONSTRAINT signup_approvals_removal_lease_check CHECK (
        (removal_lease_until IS NULL) = (removal_lease_token IS NULL));

CREATE INDEX signup_approvals_removal_due ON signup_approvals (removal_next_at)
    WHERE removal_state = 'pending';

CREATE TABLE signup_removal_steps (
    signup_id       uuid        NOT NULL REFERENCES signup_approvals (id) ON DELETE CASCADE,
    step            text        NOT NULL CHECK (step IN ('suspend', 'members', 'licences', 'identity',
                                                         'licences_tenant')),
    ord             smallint    NOT NULL CHECK (ord BETWEEN 1 AND 5),
    status          text        NOT NULL DEFAULT 'pending'
                                CHECK (status IN ('pending', 'ok', 'skipped', 'failed', 'blocked')),
    attempts        integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Machine code of the last failure, block or skip; NULL when ok.
    code            text,
    -- Short human detail (no personal data, no secrets).
    message         text        CHECK (message IS NULL OR char_length(message) <= 500),
    -- What the step did: licence ids revoked / released, member count, the login outcome. Never a token or email.
    data            jsonb       NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(data) = 'object'),
    last_attempt_at timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (signup_id, step),
    UNIQUE (signup_id, ord)
);

REVOKE DELETE, TRUNCATE ON signup_removal_steps FROM carebus_app;
GRANT SELECT, INSERT, UPDATE ON signup_removal_steps TO carebus_app;
