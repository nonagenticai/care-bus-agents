-- Sources: customer data feeds that care-bus re-syncs into strata_rag through strata_ingest.
-- Credentials are AES-256-GCM ciphertext (carebus-api `crypto`), never plaintext; the nonce is stored beside them.
CREATE TABLE sources (
    id                uuid PRIMARY KEY,
    name              text NOT NULL UNIQUE CHECK (length(btrim(name)) BETWEEN 1 AND 120),
    type              text NOT NULL CHECK (type IN ('website', 'api', 'file', 'datatable')),
    config            jsonb NOT NULL DEFAULT '{}' CHECK (jsonb_typeof(config) = 'object'),
    mapping           jsonb NOT NULL DEFAULT '[]' CHECK (jsonb_typeof(mapping) = 'array'),  -- [{source_field, entity, attribute}]
    target_store_id   uuid NOT NULL,                                                         -- a strata_rag LEAF store
    schedule          text CHECK (schedule IS NULL OR length(schedule) BETWEEN 1 AND 100),  -- cron; stored and shown, not run
    enabled           boolean NOT NULL DEFAULT true,
    credentials_enc   bytea,
    credentials_nonce bytea,
    created_at        timestamptz NOT NULL DEFAULT now(),
    updated_at        timestamptz NOT NULL DEFAULT now(),
    updated_by        text NOT NULL,
    CHECK ((credentials_enc IS NULL) = (credentials_nonce IS NULL)),
    CHECK (credentials_nonce IS NULL OR length(credentials_nonce) = 12)
);
CREATE INDEX sources_target_store ON sources (target_store_id);

-- One row per re-sync. `document_ids` are strata_rag document ids the run's ingest jobs produced;
-- `documents_removed` turns true once a later run (or a purge) deleted them from strata_rag.
CREATE TABLE source_runs (
    id                uuid PRIMARY KEY,
    source_id         uuid NOT NULL REFERENCES sources(id) ON DELETE CASCADE,
    status            text NOT NULL CHECK (status IN ('running', 'completed', 'failed', 'unsupported')),
    started_at        timestamptz NOT NULL DEFAULT clock_timestamp(),
    finished_at       timestamptz,
    started_by        text NOT NULL,
    ingest_job_ids    text[] NOT NULL DEFAULT '{}',
    document_ids      text[] NOT NULL DEFAULT '{}',
    error             text,
    replaced_run_id   uuid REFERENCES source_runs(id) ON DELETE SET NULL,
    replace_report    jsonb,                          -- {deleted, already_gone, kept_shared, failed[]}
    documents_removed boolean NOT NULL DEFAULT false,
    CHECK ((status = 'running') = (finished_at IS NULL))
);
CREATE UNIQUE INDEX source_runs_one_running ON source_runs (source_id) WHERE status = 'running';
CREATE INDEX source_runs_by_source ON source_runs (source_id, started_at DESC, id DESC);
