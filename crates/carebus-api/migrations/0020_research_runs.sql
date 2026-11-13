-- Plan 18: long-running research / legal-research runs. The `researcher` bin claims a queued run, drives a
-- model-chosen tool loop and checkpoints every step here, so a run survives a restart and resumes from its last
-- step. Claims and citations are written by the model; `verify_status` is written ONLY by the deterministic
-- verifier (research::verify), never by the model. Numbered 0020: 0019 is plan 17's signup_approvals.
-- Grants come from 0007's default privileges.

-- Two more provider uses: the research loop (plan/read/act) and the final memo writer. Verification is code.
ALTER TABLE llm_provider_uses DROP CONSTRAINT llm_provider_uses_use_check;
ALTER TABLE llm_provider_uses ADD CONSTRAINT llm_provider_uses_use_check
    CHECK ("use" IN ('qualify', 'gates', 'research', 'research_write'));

CREATE TABLE research_runs (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id        uuid NOT NULL DEFAULT app_current_job(),
    id            uuid PRIMARY KEY,
    -- 'research' = generic cited research; 'legal' = Polish legal research (ELI/SAOS/EUR-Lex verified cites).
    profile       text NOT NULL CHECK (profile IN ('research', 'legal')),
    question      text NOT NULL CHECK (length(btrim(question)) BETWEEN 1 AND 8000),
    -- strata store ids (uuid strings) the run may search; empty = none.
    store_ids     text[] NOT NULL DEFAULT '{}',
    status        text NOT NULL DEFAULT 'queued'
                  CHECK (status IN ('queued', 'running', 'verifying', 'done', 'failed', 'cancelled')),
    -- {max_steps, max_tool_calls, max_wall_s, max_tokens}; the runner fills defaults for missing keys.
    budget        jsonb NOT NULL DEFAULT '{}',
    steps_used    integer NOT NULL DEFAULT 0,
    tokens_used   bigint NOT NULL DEFAULT 0,
    -- Final memo (markdown) after verification: unverifiable claims are removed or marked, never kept silently.
    report        text,
    error         text,
    cancel_requested boolean NOT NULL DEFAULT false,
    -- Lease: the runner that owns the run and until when (a crashed runner's run is re-claimable after it).
    lease_owner   text,
    lease_until   timestamptz,
    -- Claims so far (every carebus_research_claim adds one); the runner fails a run past its maximum
    -- (RESEARCHER_MAX_ATTEMPTS, error 'too_many_attempts') so a run that crashes its runner cannot loop forever.
    attempts      integer NOT NULL DEFAULT 0,
    created_by    text NOT NULL,
    created_at    timestamptz NOT NULL DEFAULT now(),
    started_at    timestamptz,
    finished_at   timestamptz,
    updated_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT research_runs_tenant_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CONSTRAINT research_runs_tenant_id_key UNIQUE (tenant_id, id)
);
CREATE INDEX research_runs_claimable ON research_runs (created_at) WHERE status IN ('queued', 'running', 'verifying');

-- One row per loop step: a model turn (kind 'model') or one tool call (kind 'tool'). `ordinal` is dense per run.
-- Tool results are kept verbatim (capped by the runner) because the verifier matches quotes against them.
CREATE TABLE research_steps (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant(),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    run_id      uuid NOT NULL,
    ordinal     integer NOT NULL CHECK (ordinal >= 0),
    kind        text NOT NULL CHECK (kind IN ('model', 'tool')),
    -- model: the assistant message as sent back to the model (content + tool_calls).
    -- tool: {"tool_call_id", "name", "arguments"}.
    request     jsonb NOT NULL,
    -- model: usage + finish_reason. tool: {"ok": bool, "text": "...", "truncated": bool}.
    response    jsonb,
    status      text NOT NULL CHECK (status IN ('ok', 'error', 'denied')),
    duration_ms integer,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (run_id, ordinal),
    CONSTRAINT research_steps_run_fkey FOREIGN KEY (tenant_id, run_id) REFERENCES research_runs (tenant_id, id)
        ON DELETE CASCADE
);

-- A cited source. `step_ordinal` is the tool step whose result the quote must appear in.
-- source_kind: 'strata' (locator = chunk/document id), 'web' (url), 'eli' (ELI id e.g. DU/1964/59),
-- 'saos' (judgment id), 'eurlex' (CELEX), 'krs' (KRS number), 'other'.
CREATE TABLE research_citations (
    tenant_id     uuid NOT NULL DEFAULT app_current_tenant(),
    job_id        uuid NOT NULL DEFAULT app_current_job(),
    run_id        uuid NOT NULL,
    id            uuid PRIMARY KEY,
    source_kind   text NOT NULL CHECK (source_kind IN ('strata', 'web', 'eli', 'saos', 'eurlex', 'krs', 'other')),
    locator       text NOT NULL CHECK (length(locator) BETWEEN 1 AND 2000),
    title         text,
    quote         text NOT NULL CHECK (length(btrim(quote)) BETWEEN 1 AND 4000),
    step_ordinal  integer,
    verify_status text NOT NULL DEFAULT 'pending'
                  CHECK (verify_status IN ('pending', 'verified', 'quote_mismatch', 'source_unresolved',
                                           'not_in_force', 'unverifiable')),
    verify_detail jsonb,
    verified_at   timestamptz,
    created_at    timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT research_citations_run_fkey FOREIGN KEY (tenant_id, run_id) REFERENCES research_runs (tenant_id, id)
        ON DELETE CASCADE,
    CONSTRAINT research_citations_tenant_id_key UNIQUE (tenant_id, id)
);
CREATE INDEX research_citations_run ON research_citations (run_id);

-- A claim in the memo and the citations that support it. A claim is 'supported' only when at least one of its
-- citations is 'verified'; that status is computed by the verifier.
CREATE TABLE research_claims (
    tenant_id    uuid NOT NULL DEFAULT app_current_tenant(),
    job_id       uuid NOT NULL DEFAULT app_current_job(),
    run_id       uuid NOT NULL,
    id           uuid PRIMARY KEY,
    ordinal      integer NOT NULL,
    text         text NOT NULL CHECK (length(btrim(text)) BETWEEN 1 AND 4000),
    citation_ids uuid[] NOT NULL DEFAULT '{}',
    status       text NOT NULL DEFAULT 'pending' CHECK (status IN ('pending', 'supported', 'unsupported')),
    created_at   timestamptz NOT NULL DEFAULT now(),
    CONSTRAINT research_claims_run_fkey FOREIGN KEY (tenant_id, run_id) REFERENCES research_runs (tenant_id, id)
        ON DELETE CASCADE,
    UNIQUE (run_id, ordinal)
);

-- RLS as 0007: tenant isolation + restrictive job scope.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['research_runs', 'research_steps', 'research_citations', 'research_claims'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
    END LOOP;
END $$;

-- The runner is not a tenant: this definer function is its one cross-tenant read+write. It leases the oldest
-- claimable run (queued, or running/verifying with an expired lease = a crashed runner) and returns only the
-- scope needed to continue under RLS (tenant, job, run id). Everything after runs through begin_scoped.
CREATE FUNCTION carebus_research_claim(p_owner text, p_lease_s integer)
    RETURNS TABLE (tenant_id uuid, job_id uuid, id uuid)
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    BEGIN
        IF p_owner IS NULL OR length(p_owner) = 0 OR p_lease_s IS NULL OR p_lease_s < 10 OR p_lease_s > 3600 THEN
            RAISE EXCEPTION 'carebus_research_claim: owner and a lease of 10..3600 s are required'
                USING ERRCODE = 'invalid_parameter_value';
        END IF;
        RETURN QUERY
        UPDATE research_runs r
           SET lease_owner = p_owner,
               lease_until = now() + make_interval(secs => p_lease_s),
               status      = CASE WHEN r.status = 'queued' THEN 'running' ELSE r.status END,
               started_at  = coalesce(r.started_at, now()),
               attempts    = r.attempts + 1,
               updated_at  = now()
         WHERE r.id = (
                SELECT c.id FROM research_runs c
                 WHERE c.status = 'queued'
                    OR (c.status IN ('running', 'verifying') AND (c.lease_until IS NULL OR c.lease_until < now()))
                 ORDER BY c.created_at
                 LIMIT 1
                 FOR UPDATE SKIP LOCKED)
        RETURNING r.tenant_id, r.job_id, r.id;
    END
    $$;

REVOKE ALL ON FUNCTION carebus_research_claim(text, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_research_claim(text, integer) TO carebus_app;
