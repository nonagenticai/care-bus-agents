-- W3 plan 10 Task 2: row-level security on every tenant table, the runtime role, and the two definer lookups.
-- Migrations run as the database owner, which bypasses RLS on its own tables (no FORCE). The API serves through
-- `carebus_app` (NOSUPERUSER, NOBYPASSRLS), which sees exactly the rows of the tenant in the transaction-local GUC
-- `app.current_tenant` and, when `app.current_job` is set, only that job's rows. A session-level GUC is never used:
-- pgbouncer runs this database in session mode, so it would leak to the next client of that server connection.

CREATE FUNCTION app_current_tenant() RETURNS uuid
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT nullif(current_setting('app.current_tenant', true), '')::uuid $$;
CREATE FUNCTION app_current_job() RETURNS uuid
    LANGUAGE sql STABLE PARALLEL SAFE
    AS $$ SELECT nullif(current_setting('app.current_job', true), '')::uuid $$;

-- Rows written under a scoped transaction land in that scope without naming it.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['model_routes', 'connectors', 'connector_field_mappings', 'context_spaces',
                             'ontology_entities', 'ontology_attributes', 'ontology_relations', 'procedures',
                             'procedure_steps', 'procedure_executions', 'audit_events', 'case_history',
                             'sources', 'source_runs'] LOOP
        EXECUTE format('ALTER TABLE %I ALTER COLUMN tenant_id SET DEFAULT app_current_tenant(), '
                       'ALTER COLUMN job_id SET DEFAULT app_current_job()', t);
    END LOOP;
END $$;
ALTER TABLE users ALTER COLUMN tenant_id SET DEFAULT app_current_tenant();
ALTER TABLE jobs ALTER COLUMN tenant_id SET DEFAULT app_current_tenant();

-- The runtime role. Production creates it LOGIN with a password before this runs (gitops provision-service-db.sh
-- APP_ROLE=carebus_app); a fresh test server gets it NOLOGIN here and the test harness grants LOGIN. A migration
-- owner without CREATEROLE skips creation, and the GRANTs below then fail loudly instead of serving without a role.
-- Test binaries migrate many databases of one server concurrently, so a lost CREATE ROLE race is not an error.
DO $$
BEGIN
    IF NOT EXISTS (SELECT 1 FROM pg_roles WHERE rolname = 'carebus_app') THEN
        CREATE ROLE carebus_app NOLOGIN NOSUPERUSER NOCREATEDB NOCREATEROLE NOBYPASSRLS;
    END IF;
EXCEPTION
    WHEN duplicate_object OR unique_violation THEN NULL;
    WHEN insufficient_privilege THEN
        RAISE NOTICE 'carebus_app not created: the migration owner lacks CREATEROLE';
END $$;

GRANT USAGE ON SCHEMA public TO carebus_app;
GRANT SELECT, INSERT, UPDATE, DELETE ON ALL TABLES IN SCHEMA public TO carebus_app;
GRANT USAGE, SELECT ON ALL SEQUENCES IN SCHEMA public TO carebus_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT SELECT, INSERT, UPDATE, DELETE ON TABLES TO carebus_app;
ALTER DEFAULT PRIVILEGES IN SCHEMA public GRANT USAGE, SELECT ON SEQUENCES TO carebus_app;
REVOKE ALL ON super_admin_active FROM carebus_app;
REVOKE ALL ON _sqlx_migrations FROM carebus_app;

-- Tenant isolation grants access at all (permissive); the job filter only narrows it (restrictive). Job isolation is
-- not a security boundary (a tenant reaches all its jobs): it removes a `job_id = $n` predicate from every query.
-- Child tables carry their own tenant_id/job_id, so no policy relies on a join to its parent.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['model_routes', 'connectors', 'connector_field_mappings', 'context_spaces',
                             'ontology_entities', 'ontology_attributes', 'ontology_relations', 'procedures',
                             'procedure_steps', 'procedure_executions', 'audit_events', 'case_history',
                             'sources', 'source_runs', 'users', 'jobs'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        IF t NOT IN ('users', 'jobs') THEN
            EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                           'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                           'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
        END IF;
    END LOOP;
END $$;

ALTER TABLE tenants ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_self ON tenants FOR ALL
    USING (id = app_current_tenant()) WITH CHECK (id = app_current_tenant());

-- Definer-only: RLS on, no policy, no grant.
ALTER TABLE super_admin_active ENABLE ROW LEVEL SECURITY;

-- The only reads before a tenant is known. Owned by the migration owner, which bypasses RLS on its tables.
-- pg_temp is listed last so a caller's temporary table cannot shadow `users` or `tenants`.
CREATE FUNCTION carebus_auth_user(p_sub text)
    RETURNS TABLE (tenant_id uuid, tenant_role text, tenant_status text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT u.tenant_id, u.tenant_role, t.status
          FROM users u JOIN tenants t ON t.id = u.tenant_id
         WHERE u.sub = p_sub
    $$;

-- Records a super_admin's active tenant. Returns the previous tenant (NULL on the first call), or p_tenant itself
-- when it is unchanged, so the caller audits a switch exactly when the result differs from p_tenant.
-- Concurrent first calls serialise on the primary key: only one of them sees NULL.
CREATE FUNCTION carebus_switch_active_tenant(p_sub text, p_tenant uuid)
    RETURNS uuid
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    DECLARE
        prev uuid;
    BEGIN
        IF p_tenant IS NULL THEN
            RAISE EXCEPTION 'carebus_switch_active_tenant: tenant is required' USING ERRCODE = 'null_value_not_allowed';
        END IF;
        INSERT INTO super_admin_active (sub, tenant_id, switched_at) VALUES (p_sub, p_tenant, now())
            ON CONFLICT (sub) DO NOTHING;
        IF FOUND THEN
            RETURN NULL;
        END IF;
        SELECT a.tenant_id INTO prev FROM super_admin_active a WHERE a.sub = p_sub FOR UPDATE;
        IF prev = p_tenant THEN
            RETURN p_tenant;
        END IF;
        UPDATE super_admin_active SET tenant_id = p_tenant, switched_at = now() WHERE sub = p_sub;
        RETURN prev;
    END
    $$;

REVOKE ALL ON FUNCTION carebus_auth_user(text) FROM PUBLIC;
REVOKE ALL ON FUNCTION carebus_switch_active_tenant(text, uuid) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_auth_user(text) TO carebus_app;
GRANT EXECUTE ON FUNCTION carebus_switch_active_tenant(text, uuid) TO carebus_app;
