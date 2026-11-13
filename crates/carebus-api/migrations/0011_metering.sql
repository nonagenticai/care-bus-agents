-- W3 plan 15: the usage ledger and the metering outbox (transactional outbox, commonpractice
-- `host/metering-outbox-drainer.ts`). Both are written only with METERING_EMIT_ENABLED=true: one ledger row per model
-- call of a chat turn and its outbox row, in one transaction under the job's RLS GUCs. The drainer sends each pending
-- outbox row to `billing:costs` and marks it sent; billing dedups on `event_key`. Grants come from 0007's default
-- privileges.

CREATE TABLE usage_ledger (
    id              bigserial PRIMARY KEY,
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    event_key       uuid NOT NULL,
    occurred_at     timestamptz NOT NULL DEFAULT now(),
    source          text NOT NULL CHECK (source IN ('chat_qualifier')),
    conversation_id text,
    provider        text NOT NULL,
    model           text NOT NULL,
    input_tokens    bigint NOT NULL CHECK (input_tokens >= 0),
    output_tokens   bigint NOT NULL CHECK (output_tokens >= 0),
    calls           integer NOT NULL CHECK (calls >= 0),
    latency_ms      bigint NOT NULL CHECK (latency_ms >= 0),
    license_id      text,
    subject_id      text,
    CONSTRAINT usage_ledger_event_key_key UNIQUE (event_key),
    CONSTRAINT usage_ledger_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE INDEX usage_ledger_scope_time ON usage_ledger (tenant_id, job_id, occurred_at);

CREATE TABLE metering_outbox (
    id              bigserial PRIMARY KEY,
    tenant_id       uuid NOT NULL DEFAULT app_current_tenant(),
    job_id          uuid NOT NULL DEFAULT app_current_job(),
    event_key       uuid NOT NULL,
    fields          jsonb NOT NULL CHECK (jsonb_typeof(fields) = 'object'),
    status          text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'sent')),
    attempts        integer NOT NULL DEFAULT 0 CHECK (attempts >= 0),
    next_attempt_at timestamptz NOT NULL DEFAULT now(),
    last_error      text,
    created_at      timestamptz NOT NULL DEFAULT now(),
    sent_at         timestamptz,
    CHECK ((status = 'sent') = (sent_at IS NOT NULL)),
    CONSTRAINT metering_outbox_event_key_key UNIQUE (event_key),
    CONSTRAINT metering_outbox_ledger_fkey FOREIGN KEY (event_key) REFERENCES usage_ledger (event_key),
    CONSTRAINT metering_outbox_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);
CREATE INDEX metering_outbox_due ON metering_outbox (tenant_id, job_id, next_attempt_at) WHERE status = 'pending';

-- RLS exactly as 0007/0009.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['usage_ledger', 'metering_outbox'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
    END LOOP;
END $$;
