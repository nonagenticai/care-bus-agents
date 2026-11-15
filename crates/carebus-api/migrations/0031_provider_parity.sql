-- D38b: provider parity with `agent` (agent's 0050_llm_providers, which was itself ported from this repo). The
-- tenant's LLM/GPU provider rows gain the columns the richer UI and the sealed keyring need, and removal becomes a
-- SOFT delete so a binding that still names a removed provider resolves to an explicit 409 rather than a silent
-- fallback to the platform default.
--
-- `secret_sealed` is `v2:<kid>:base64url(nonce || ciphertext || tag)` sealed under PROVIDER_SECRET_KEYS
-- (crate::provider_keyring), AAD `<tenant>:llm_key:<id>` || kid, with its fingerprint in `secret_fpr`; both are set
-- or neither. `secret_enc` is NOT dropped: it holds rows sealed before this migration under CAREBUS_SOURCE_KEY with
-- AAD = the provider id, and the read path still opens it, so no live credential is invalidated here.
--
-- `models` is the provider's declared catalogue (the values a use may bind to). It may be empty on a legacy row,
-- because this repo records the model on the binding (`llm_provider_uses.model`) and three production rows predate
-- the column; the API requires at least one model on write. `last_test` holds the most recent `/test` result.
--
-- Forward-only and drift-tolerant as 0015/0020: every column is IF NOT EXISTS and every constraint is
-- DROP IF EXISTS + ADD.

ALTER TABLE llm_providers
    ADD COLUMN IF NOT EXISTS models            text[] NOT NULL DEFAULT '{}',
    ADD COLUMN IF NOT EXISTS region            text,
    ADD COLUMN IF NOT EXISTS secret_sealed     text,
    ADD COLUMN IF NOT EXISTS secret_fpr        text,
    ADD COLUMN IF NOT EXISTS secret_updated_at timestamptz,
    ADD COLUMN IF NOT EXISTS last_test         jsonb,
    ADD COLUMN IF NOT EXISTS deleted_at        timestamptz;

ALTER TABLE llm_providers
    DROP CONSTRAINT IF EXISTS llm_providers_models_check,
    ADD CONSTRAINT llm_providers_models_check
        CHECK (cardinality(models) <= 100 AND array_position(models, NULL) IS NULL),
    DROP CONSTRAINT IF EXISTS llm_providers_region_check,
    ADD CONSTRAINT llm_providers_region_check
        CHECK (region IS NULL OR length(btrim(region)) BETWEEN 1 AND 64),
    -- A sealed secret always carries its fingerprint and its timestamp, and never survives a soft delete.
    DROP CONSTRAINT IF EXISTS llm_providers_secret_check,
    ADD CONSTRAINT llm_providers_secret_check CHECK (
        (secret_sealed IS NULL OR secret_sealed LIKE 'v2:%')
        AND (secret_sealed IS NULL) = (secret_fpr IS NULL)
        AND (secret_sealed IS NULL) = (secret_updated_at IS NULL)),
    DROP CONSTRAINT IF EXISTS llm_providers_deleted_check,
    ADD CONSTRAINT llm_providers_deleted_check
        CHECK (deleted_at IS NULL OR secret_sealed IS NULL),
    DROP CONSTRAINT IF EXISTS llm_providers_last_test_check,
    ADD CONSTRAINT llm_providers_last_test_check
        CHECK (last_test IS NULL OR jsonb_typeof(last_test) = 'object');

-- A name is unique among a tenant's LIVE providers only, so a soft-deleted provider frees its name for reuse.
ALTER TABLE llm_providers DROP CONSTRAINT IF EXISTS llm_providers_name_key;
CREATE UNIQUE INDEX IF NOT EXISTS llm_providers_tenant_name_live
    ON llm_providers (tenant_id, name) WHERE deleted_at IS NULL;
