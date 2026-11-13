-- Plan 18 follow-ups. Numbered 0023: 0019-0022 are plans 17/18.
--
-- 1. Research runs are metered: every model call of the `researcher` lands in `usage_ledger` with source 'research'
--    and the run it belongs to (research::runner, through crate::metering as the chat qualifier does).
-- 2. Tenant LLM settings, next to `llm_providers`: `external_llm_allowed` (default false). While false, the
--    researcher fails a run whose model resolves to anything but the platform's own model (legal compliance: default
--    to self-hosted EU inference). It applies ONLY to research runs for now; chat qualification and gates ignore it.
-- Grants come from 0007's default privileges.

ALTER TABLE usage_ledger DROP CONSTRAINT usage_ledger_source_check;
ALTER TABLE usage_ledger ADD CONSTRAINT usage_ledger_source_check
    CHECK (source IN ('chat_qualifier', 'research'));
ALTER TABLE usage_ledger ADD COLUMN research_run_id uuid;
-- A research row names its run; no other row does.
ALTER TABLE usage_ledger ADD CONSTRAINT usage_ledger_research_run_check
    CHECK ((source = 'research') = (research_run_id IS NOT NULL));
-- A run's spend is read by run id. No foreign key to research_runs on purpose: ledger rows are billing records and
-- must outlive the run they belong to (a deleted run never takes its usage with it).
CREATE INDEX usage_ledger_research_run_idx ON usage_ledger (research_run_id) WHERE research_run_id IS NOT NULL;

-- One row per tenant; no row reads as the defaults (external_llm_allowed = false).
CREATE TABLE tenant_llm_settings (
    tenant_id            uuid PRIMARY KEY DEFAULT app_current_tenant() REFERENCES tenants (id),
    external_llm_allowed boolean NOT NULL DEFAULT false,
    updated_at           timestamptz NOT NULL DEFAULT now(),
    updated_by           text NOT NULL
);

-- RLS as 0015: tenant-level only (no job_id: the settings are the tenant's, shared by its jobs).
ALTER TABLE tenant_llm_settings ENABLE ROW LEVEL SECURITY;
CREATE POLICY tenant_isolation ON tenant_llm_settings FOR ALL
    USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant());
