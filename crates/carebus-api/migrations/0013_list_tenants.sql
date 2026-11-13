-- W3 plan 14: the super_admin tenant picker lists every tenant. `tenants` stays under `tenant_self` RLS (a request
-- sees only its own tenant row); this definer function is the one read across tenants, and only id, name and
-- status leave it. The route calling it (`GET /api/tenants`) is super_admin only. Numbered 0013: 0011 is plan 15's
-- metering, 0012 is reserved for plan 12.
CREATE FUNCTION carebus_list_tenants()
    RETURNS TABLE (id uuid, name text, status text)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT t.id, t.name, t.status FROM tenants t ORDER BY t.name, t.id
    $$;

REVOKE ALL ON FUNCTION carebus_list_tenants() FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_list_tenants() TO carebus_app;
