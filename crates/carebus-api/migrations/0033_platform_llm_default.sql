-- 0033: the platform default becomes data.
--
-- Until now the deployment's own qualifier LLM was environment only (`VLLM_LICENSE_TOKEN`,
-- `CAREBUS_LLM_BASE_URL`, `CAREBUS_MODEL`). Those variables are now bootstrap: carebus-api's boot
-- inserts this row once from them — only when it is absent, so a super_admin's edit is never
-- reverted — and every runtime read comes from here, so the platform model can change without a
-- redeploy. The researcher reads the same row for its `DefaultLlm`.
--
-- ONE row: a boolean primary key that only `true` can satisfy. NO row-level security: the row is
-- the platform's own configuration, not tenant data, so there is no tenant to isolate it by; the
-- write is super_admin-only at the route and the app role needs the DML grant below (0007's
-- `ALTER DEFAULT PRIVILEGES` covers owner-created tables, this restates it for the reader).
--
-- The secret is sealed under `platform:llm_key:default` (crate::provider_secret), never the tenant
-- scope, and keeps 0031's all-or-none shape: sealed, fingerprint and timestamp are set together.

CREATE TABLE IF NOT EXISTS platform_llm_default (
    id                boolean     PRIMARY KEY DEFAULT true CHECK (id),
    base_url          text        NOT NULL CHECK (base_url ~ '^https?://'),
    model             text        NOT NULL CHECK (length(btrim(model)) BETWEEN 1 AND 200),
    secret_sealed     text,
    secret_fpr        text,
    secret_updated_at timestamptz,
    version           bigint      NOT NULL DEFAULT 1,
    updated_at        timestamptz NOT NULL DEFAULT now(),
    updated_by        text        NOT NULL DEFAULT 'bootstrap',
    CONSTRAINT platform_llm_default_secret_check CHECK (
        (secret_sealed IS NULL OR secret_sealed LIKE 'v2:%')
        AND (secret_sealed IS NULL) = (secret_fpr IS NULL)
        AND (secret_sealed IS NULL) = (secret_updated_at IS NULL))
);

GRANT SELECT, INSERT, UPDATE ON platform_llm_default TO carebus_app;
