-- W3 plan 13 Task 3: every scope the agents serve, for the API's startup rebuild of the Redis scope registry.
-- A scope is one (tenant, job key) of an active tenant; a suspended tenant's jobs are not served. The API reads it
-- before any tenant is known, so it is a definer function (owned by the migration owner, which bypasses RLS on its
-- own tables) instead of a cross-tenant policy. pg_temp is listed last so a temporary table cannot shadow a table.
CREATE FUNCTION carebus_all_scopes()
    RETURNS TABLE (tenant_id uuid, job_key text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT j.tenant_id, j.key
          FROM jobs j JOIN tenants t ON t.id = j.tenant_id
         WHERE t.status = 'active'
         ORDER BY j.tenant_id, j.key
    $$;

REVOKE ALL ON FUNCTION carebus_all_scopes() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_all_scopes() TO carebus_app;
