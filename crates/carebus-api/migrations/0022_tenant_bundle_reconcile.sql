-- care-bus 17: tenants that existed before early access (the bootstrap tenant among them) hold no care-bus licence
-- set yet, so per-tenant strata tokens (CAREBUS_STRATA_BUNDLE_TOKENS) would 409 for them. With
-- CAREBUS_TENANT_BUNDLE_RECONCILE=true the signup tick registers every ACTIVE tenant in licences and ensures its
-- care-bus bundle once (crate::signup::provisioning::reconcile_bundles); this table records it per tenant, with the
-- retry backoff of a failure. Tenant data under RLS like every table with a tenant_id: the tick reads and writes a
-- tenant's row only inside that tenant's context. The API never deletes it.
CREATE TABLE tenant_bundle_reconcile (
    tenant_id       uuid        PRIMARY KEY DEFAULT app_current_tenant() REFERENCES tenants (id) ON DELETE CASCADE,
    status          text        NOT NULL CHECK (status IN ('pending', 'ok', 'failed')),
    attempts        integer     NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    -- Machine-ish failure detail (no secrets, no personal data); NULL when ok.
    message         text        CHECK (message IS NULL OR char_length(message) <= 500),
    next_attempt_at timestamptz,
    -- The per-tenant claim, so two replicas never ensure the same tenant at once.
    lease_until     timestamptz,
    updated_at      timestamptz NOT NULL DEFAULT now()
);
ALTER TABLE tenant_bundle_reconcile ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_bundle_reconcile FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
REVOKE DELETE, TRUNCATE ON tenant_bundle_reconcile FROM carebus_app;
GRANT SELECT, INSERT, UPDATE ON tenant_bundle_reconcile TO carebus_app;

-- The tick's one read across tenants: the ACTIVE tenants still to reconcile (no row yet, or `pending`, due and with
-- its lease free), oldest due first, at most `p_limit`; only ids leave it. An `ok` or `failed` tenant is never
-- returned, so a settled estate costs a tick this one query and no transaction per tenant. The seed and the claim
-- still happen inside each tenant's own context.
CREATE FUNCTION carebus_bundle_reconcile_due(p_limit integer)
    RETURNS TABLE (id uuid)
    LANGUAGE sql STABLE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
        SELECT t.id
          FROM tenants t
          LEFT JOIN tenant_bundle_reconcile r ON r.tenant_id = t.id
         WHERE t.status = 'active'
           AND (r.tenant_id IS NULL
                OR (r.status = 'pending' AND r.next_attempt_at <= now()
                    AND (r.lease_until IS NULL OR r.lease_until < now())))
         ORDER BY r.next_attempt_at NULLS FIRST, t.id
         LIMIT greatest(p_limit, 0)
    $$;

REVOKE ALL ON FUNCTION carebus_bundle_reconcile_due(integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_bundle_reconcile_due(integer) TO carebus_app;
