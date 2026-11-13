-- Plan 18, legal compliance (ethical walls): legal research is scoped to a MATTER with per-user access, beyond the
-- tenant/job isolation of 0007/0020. Numbered 0026: 0019-0024 are plans 17/18, 0025 is the bundle reconcile fix.
--
-- Access model (enforced here in RLS, and again in routes::legal_matters / routes::research):
--   * A user sees a matter, and the research runs filed under it with their steps, claims and citations, ONLY while
--     they are a member of that matter (role 'lead' or 'member').
--   * A tenant_admin (and a super_admin acting in the tenant) sees the matter LIST (name, reference, members, stores)
--     and may manage membership (but not grant it to themselves), and does NOT read the runs of a matter unless a
--     lead has made them a member. This mirrors lawyer
--     confidentiality: administering the firm's tool is not a licence to read a client file.
--   * A run without a matter (profile 'research') keeps today's tenant/job visibility.
--
-- The caller is named by two more transaction-local GUCs, set next to the tenant/job GUCs by tenancy::TenantCtx /
-- JobCtx::begin for EVERY request: `app.current_sub` (the token subject) and `app.current_sub_admin` ('1' for a
-- tenant_admin / super_admin). Unset `app.current_sub` is the system scope, exactly as unset `app.current_job` means
-- every job: the `researcher` worker (and other non-request code: recorder, metering) opens its transactions with
-- tenancy::begin_scoped, which never sets a subject, so it reads and writes the runs it claims as before (0020).
-- Grants on the tables come from 0007's default privileges.

CREATE FUNCTION app_current_sub() RETURNS text
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT nullif(current_setting('app.current_sub', true), '') $$;
CREATE FUNCTION app_current_sub_is_admin() RETURNS boolean
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT coalesce(current_setting('app.current_sub_admin', true), '') = '1' $$;

CREATE TABLE legal_matters (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id     uuid NOT NULL DEFAULT app_current_job(),
    id         uuid PRIMARY KEY,
    name       text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 200),
    -- The firm's own file number (sygnatura / numer sprawy), unique per job when given.
    reference  text CHECK (reference IS NULL OR length(btrim(reference)) BETWEEN 1 AND 100),
    created_by text NOT NULL,
    created_at timestamptz NOT NULL DEFAULT now(),
    closed_at  timestamptz,
    CONSTRAINT legal_matters_tenant_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CONSTRAINT legal_matters_tenant_id_key UNIQUE (tenant_id, id)
);
CREATE UNIQUE INDEX legal_matters_reference ON legal_matters (tenant_id, job_id, reference) WHERE reference IS NOT NULL;

-- Who may work on a matter. A member must be a user of the tenant (checked by the route; the FK only removes the
-- membership when the user is deleted).
CREATE TABLE legal_matter_members (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL DEFAULT app_current_job(),
    matter_id uuid NOT NULL,
    user_sub  text NOT NULL REFERENCES users (sub) ON DELETE CASCADE,
    role      text NOT NULL CHECK (role IN ('lead', 'member')),
    added_by  text NOT NULL,
    added_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (matter_id, user_sub),
    CONSTRAINT legal_matter_members_matter_fkey FOREIGN KEY (tenant_id, matter_id)
        REFERENCES legal_matters (tenant_id, id) ON DELETE CASCADE
);
CREATE INDEX legal_matter_members_sub ON legal_matter_members (user_sub);

-- The strata stores (knowledge stores of the job) a matter's runs may search. A store attached to a matter is
-- walled: only runs filed under THAT matter may search it (routes::research), and the tenant-level knowledge routes
-- hide it, and everything below it in the strata tree, from whoever is not a member (crate::walls). A store belongs
-- to at most one matter in the whole tenant (`legal_matter_stores_tenant_store_key`): otherwise anyone could start
-- a matter of their own, attach another matter's store to it and file a run that searches it. A matter that still
-- has stores cannot be deleted (RESTRICT): detach them first, so a delete never silently unwalls a store.
CREATE TABLE legal_matter_stores (
    tenant_id uuid NOT NULL DEFAULT app_current_tenant(),
    job_id    uuid NOT NULL DEFAULT app_current_job(),
    matter_id uuid NOT NULL,
    store_id  text NOT NULL CHECK (store_id ~ '^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$'),
    added_by  text NOT NULL,
    added_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (matter_id, store_id),
    CONSTRAINT legal_matter_stores_tenant_store_key UNIQUE (tenant_id, store_id),
    CONSTRAINT legal_matter_stores_matter_fkey FOREIGN KEY (tenant_id, matter_id)
        REFERENCES legal_matters (tenant_id, id) ON DELETE RESTRICT
);

-- A run filed under a matter. Required for profile 'legal' by the route (not by a CHECK: legal runs created before
-- this migration have none). A matter with runs cannot be deleted, only closed.
ALTER TABLE research_runs ADD COLUMN matter_id uuid;
ALTER TABLE research_runs ADD CONSTRAINT research_runs_matter_fkey FOREIGN KEY (tenant_id, matter_id)
    REFERENCES legal_matters (tenant_id, id) ON DELETE RESTRICT;
CREATE INDEX research_runs_matter ON research_runs (matter_id) WHERE matter_id IS NOT NULL;

-- Membership lookups for the policies. SECURITY DEFINER because legal_matter_members is itself under a policy that
-- calls them (an invoker function would recurse); each is confined to the transaction's tenant and subject.
CREATE FUNCTION app_matter_member(p_matter uuid) RETURNS boolean
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$ SELECT EXISTS (SELECT 1 FROM legal_matter_members m
                          WHERE m.matter_id = p_matter AND m.tenant_id = app_current_tenant()
                            AND m.user_sub = app_current_sub()) $$;
CREATE FUNCTION app_matter_lead(p_matter uuid) RETURNS boolean
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$ SELECT EXISTS (SELECT 1 FROM legal_matter_members m
                          WHERE m.matter_id = p_matter AND m.tenant_id = app_current_tenant()
                            AND m.user_sub = app_current_sub() AND m.role = 'lead') $$;
-- Every walled store of the transaction's tenant (EVERY job: a wall is tenant-wide, as the knowledge routes are),
-- with its matter and whether the subject may see it: a member of that matter may; anyone else, a tenant_admin
-- included, may not; no subject (the researcher worker and other system code) may. SECURITY DEFINER so the job
-- scope does not narrow it; it discloses store and matter ids only, never a matter's name or members.
CREATE FUNCTION carebus_store_walls() RETURNS TABLE (store_id text, matter_id uuid, visible boolean)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$ SELECT s.store_id, s.matter_id, app_current_sub() IS NULL OR app_matter_member(s.matter_id)
            FROM legal_matter_stores s
           WHERE s.tenant_id = app_current_tenant() $$;
REVOKE ALL ON FUNCTION carebus_store_walls() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_store_walls() TO carebus_app;
-- Whether a run filed under `p_matter` (NULL: none) may name `p_stores`: none of them is walled for another matter.
CREATE FUNCTION app_run_stores_allowed(p_matter uuid, p_stores text[]) RETURNS boolean
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$ SELECT NOT EXISTS (SELECT 1 FROM legal_matter_stores s
                              WHERE s.tenant_id = app_current_tenant() AND s.store_id = ANY (p_stores)
                                AND s.matter_id IS DISTINCT FROM p_matter) $$;
REVOKE ALL ON FUNCTION app_run_stores_allowed(uuid, text[]) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION app_run_stores_allowed(uuid, text[]) TO carebus_app;
REVOKE ALL ON FUNCTION app_matter_member(uuid) FROM PUBLIC;
REVOKE ALL ON FUNCTION app_matter_lead(uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION app_matter_member(uuid) TO carebus_app;
GRANT EXECUTE ON FUNCTION app_matter_lead(uuid) TO carebus_app;

-- Tenant isolation + restrictive job scope, as 0007/0020.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['legal_matters', 'legal_matter_members', 'legal_matter_stores'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
    END LOOP;
END $$;

-- Matters: members and admins read; lead or admin change or delete. Rows are created only through
-- carebus_legal_matter_create (the creator is not a member until the same call makes them lead).
CREATE POLICY matter_read ON legal_matters AS RESTRICTIVE FOR SELECT
    USING (app_current_sub() IS NULL OR app_current_sub_is_admin() OR app_matter_member(id));
CREATE POLICY matter_insert ON legal_matters AS RESTRICTIVE FOR INSERT
    WITH CHECK (app_current_sub() IS NULL);
CREATE POLICY matter_update ON legal_matters AS RESTRICTIVE FOR UPDATE
    USING (app_current_sub() IS NULL OR app_current_sub_is_admin() OR app_matter_lead(id))
    WITH CHECK (app_current_sub() IS NULL OR app_current_sub_is_admin() OR app_matter_lead(id));
CREATE POLICY matter_delete ON legal_matters AS RESTRICTIVE FOR DELETE
    USING (app_current_sub() IS NULL OR app_current_sub_is_admin() OR app_matter_lead(id));

-- Members: members and admins read; lead or admin add, change and remove. An admin who is not a lead cannot add
-- or promote THEMSELVES (that would be reading a client file by self-grant); a lead of the matter must do it.
CREATE POLICY member_read ON legal_matter_members AS RESTRICTIVE FOR SELECT
    USING (app_current_sub() IS NULL OR app_current_sub_is_admin() OR app_matter_member(matter_id));
CREATE POLICY member_insert ON legal_matter_members AS RESTRICTIVE FOR INSERT
    WITH CHECK (app_current_sub() IS NULL OR app_matter_lead(matter_id)
                OR (app_current_sub_is_admin() AND user_sub <> app_current_sub()));
CREATE POLICY member_update ON legal_matter_members AS RESTRICTIVE FOR UPDATE
    USING (app_current_sub() IS NULL OR app_current_sub_is_admin() OR app_matter_lead(matter_id))
    WITH CHECK (app_current_sub() IS NULL OR app_matter_lead(matter_id)
                OR (app_current_sub_is_admin() AND user_sub <> app_current_sub()));
CREATE POLICY member_delete ON legal_matter_members AS RESTRICTIVE FOR DELETE
    USING (app_current_sub() IS NULL OR app_current_sub_is_admin() OR app_matter_lead(matter_id));

-- Stores: readable in the whole job (a store id and a matter id disclose nothing, and the store wall in
-- routes::research must see which stores are walled); only the matter's lead attaches and detaches (not an admin).
CREATE POLICY store_insert ON legal_matter_stores AS RESTRICTIVE FOR INSERT
    WITH CHECK (app_current_sub() IS NULL OR app_matter_lead(matter_id));
CREATE POLICY store_update ON legal_matter_stores AS RESTRICTIVE FOR UPDATE
    USING (app_current_sub() IS NULL OR app_matter_lead(matter_id))
    WITH CHECK (app_current_sub() IS NULL OR app_matter_lead(matter_id));
CREATE POLICY store_delete ON legal_matter_stores AS RESTRICTIVE FOR DELETE
    USING (app_current_sub() IS NULL OR app_matter_lead(matter_id));

-- Runs under a matter: members only. Deliberately NO tenant_admin clause: an admin who is not a member does not
-- see, create, change or cancel a matter's runs. The worker (no subject) is unaffected.
CREATE POLICY matter_wall ON research_runs AS RESTRICTIVE FOR ALL
    USING (matter_id IS NULL OR app_current_sub() IS NULL OR app_matter_member(matter_id))
    WITH CHECK (matter_id IS NULL OR app_current_sub() IS NULL OR app_matter_member(matter_id));

-- A run (filed by a request) names only stores walled for its own matter, or none walled at all (no store above a
-- wall either: routes::research checks that against the strata tree, which the database does not know).
CREATE POLICY run_stores ON research_runs AS RESTRICTIVE FOR INSERT
    WITH CHECK (app_current_sub() IS NULL OR app_run_stores_allowed(matter_id, store_ids));

-- Steps, claims and citations follow their run: visible exactly when the run is (the subquery runs under the
-- research_runs policies above, so the matter wall applies to it).
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['research_steps', 'research_citations', 'research_claims'] LOOP
        EXECUTE format('CREATE POLICY matter_wall ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_sub() IS NULL OR EXISTS (SELECT 1 FROM research_runs r WHERE r.id = run_id)) '
                       'WITH CHECK (app_current_sub() IS NULL OR EXISTS (SELECT 1 FROM research_runs r WHERE r.id = run_id))', t);
    END LOOP;
END $$;

-- Creates a matter in the transaction's tenant and job and makes the transaction's subject its lead, in one step
-- (the creator could not pass the member-only policies before being a member). Requires all three GUCs.
CREATE FUNCTION carebus_legal_matter_create(p_id uuid, p_name text, p_reference text)
    RETURNS void
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        v_tenant uuid := app_current_tenant();
        v_job    uuid := app_current_job();
        v_sub    text := app_current_sub();
    BEGIN
        IF v_tenant IS NULL OR v_job IS NULL OR v_sub IS NULL THEN
            RAISE EXCEPTION 'carebus_legal_matter_create: tenant, job and subject must be set'
                USING ERRCODE = 'insufficient_privilege';
        END IF;
        INSERT INTO legal_matters (tenant_id, job_id, id, name, reference, created_by)
        VALUES (v_tenant, v_job, p_id, p_name, p_reference, v_sub);
        INSERT INTO legal_matter_members (tenant_id, job_id, matter_id, user_sub, role, added_by)
        VALUES (v_tenant, v_job, p_id, v_sub, 'lead', v_sub);
    END
    $$;
REVOKE ALL ON FUNCTION carebus_legal_matter_create(uuid, text, text) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_legal_matter_create(uuid, text, text) TO carebus_app;
