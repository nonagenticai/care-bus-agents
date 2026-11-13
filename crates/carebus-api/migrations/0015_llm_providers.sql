-- D38 (spec 2026-10-18 §2): tenant LLM providers and the two uses bound to them, replacing model_routes.
-- model_routes held provider/model names only (never a token) and nothing at runtime read it, so no row is carried
-- over. `secret_enc` is the provider's bearer token sealed with CAREBUS_SOURCE_KEY (the AES-256-GCM envelope of
-- `sources`, crate::crypto), bound to the provider id; it is write-only: never returned, logged or audited.
-- Grants come from 0007's default privileges.

DROP TABLE model_routes;
DROP TYPE model_use;

CREATE TABLE llm_providers (
    tenant_id  uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    id         uuid PRIMARY KEY,
    name       text NOT NULL CHECK (length(btrim(name)) BETWEEN 1 AND 120),
    kind       text NOT NULL CHECK (kind IN ('openai', 'laya')),
    base_url   text NOT NULL CHECK (base_url ~ '^https?://'),
    secret_enc text,
    updated_at timestamptz NOT NULL DEFAULT now(),
    updated_by text NOT NULL,
    CONSTRAINT llm_providers_tenant_id_key UNIQUE (tenant_id, id),
    CONSTRAINT llm_providers_name_key UNIQUE (tenant_id, name)
);

-- One binding per (tenant, use). provider_id NULL = unbound (MATCH SIMPLE skips the FK). Deleting a bound provider
-- is refused by the FK; the API answers 409 provider_in_use before it gets there.
CREATE TABLE llm_provider_uses (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    "use"       text NOT NULL CHECK ("use" IN ('qualify', 'gates')),
    provider_id uuid,
    model       text CHECK (model IS NULL OR length(btrim(model)) BETWEEN 1 AND 200),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    updated_by  text NOT NULL,
    PRIMARY KEY (tenant_id, "use"),
    CONSTRAINT llm_provider_uses_provider_fkey FOREIGN KEY (tenant_id, provider_id)
        REFERENCES llm_providers (tenant_id, id)
);

-- RLS as 0007, tenant-level only (no job_id: providers are the tenant's, shared by its jobs).
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['llm_providers', 'llm_provider_uses'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
    END LOOP;
END $$;
