-- 0034: web channel registry. A job registers a webchat channel (a key plus a hashed secret); the
-- public channel routes authenticate by it and resolve the job's scope WITHOUT a JWT (an external
-- front end, chat-bridge, has no care-bus user). One channel per job; a second PUT rotates the
-- secret. The secret is returned once, never stored in clear.
--
-- The lookup is SECURITY DEFINER because the registry must be read before any tenant GUC exists;
-- it returns only the row for the exact key and never lists. Every listing and write stays under
-- row-level security (the app role carries the tenant GUC on its request transactions).

CREATE TABLE web_channels (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    -- Generated (`wc_` + 32 hex); the channel's public identifier in the route path.
    key         text NOT NULL CHECK (key ~ '^[a-z0-9][a-z0-9_-]{0,63}$'),
    -- sha256 hex of the bearer secret. The clear value leaves the API exactly once, in the PUT.
    secret_hash text NOT NULL CHECK (secret_hash ~ '^[0-9a-f]{64}$'),
    label       text NOT NULL DEFAULT '',
    enabled     boolean NOT NULL DEFAULT true,
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, job_id),
    UNIQUE (key),
    FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id) ON DELETE CASCADE
);

ALTER TABLE web_channels ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON web_channels FOR ALL
    USING (tenant_id = app_current_tenant())
    WITH CHECK (tenant_id = app_current_tenant());

COMMENT ON TABLE web_channels IS
    'A job''s web channel: the key in /api/channel/webchat/{key}/... and the hash of its bearer secret.';

-- Resolve a channel key to its scope before any tenant GUC is set. SECURITY DEFINER (the table owner
-- bypasses RLS); exact key only, so a caller cannot enumerate channels.
CREATE FUNCTION web_channel_lookup(k text)
RETURNS TABLE (tenant_id uuid, job_key text, secret_hash text, enabled boolean)
LANGUAGE sql STABLE SECURITY DEFINER
SET search_path = public, pg_temp
AS $$
    SELECT c.tenant_id, j.key, c.secret_hash, c.enabled
      FROM web_channels c
      JOIN jobs j ON j.id = c.job_id AND j.tenant_id = c.tenant_id
     WHERE c.key = k
$$;

GRANT EXECUTE ON FUNCTION web_channel_lookup(text) TO carebus_app;
