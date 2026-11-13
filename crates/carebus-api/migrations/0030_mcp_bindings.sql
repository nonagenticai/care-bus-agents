-- MCP catalog bindings (crate::mcp_binding). nonagenticai_mcp_server reaches catalog tools, and so MCP tasks
-- (migration 0029), only for a BINDING caller: the tenant's `mcp` licence in X-Tenant-Id plus a binding token in
-- X-MCP-Binding (nonagenticai_mcp_server docs/AUTHENTICATION_BINDINGS.md). carebus-api provisions one binding per
-- (tenant, product) with `POST /mcp/internal/tenants/{tenant}/bindings/ensure`; the server returns the token once, so
-- it is kept here, sealed with CAREBUS_SOURCE_KEY (AES-256-GCM, the binding row as AAD). Never logged.
-- Grants come from 0007's default privileges.

CREATE TABLE mcp_bindings (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    -- The product the server provisioned it for (`care-bus`); its toolkits come from the server's
    -- MCP_BINDING_PRODUCTS.
    product     text NOT NULL CHECK (product IN ('care-bus')),
    binding_id  uuid NOT NULL,
    -- The tenant `mcp` licence the binding is pinned to (its gateway licence).
    licence_id  uuid NOT NULL,
    -- base64(nonce || ciphertext) of the binding token (crate::mcp_binding::seal).
    token_enc   text NOT NULL CHECK (length(token_enc) BETWEEN 1 AND 4000),
    created_at  timestamptz NOT NULL DEFAULT now(),
    updated_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (tenant_id, product)
);

ALTER TABLE mcp_bindings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON mcp_bindings FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
