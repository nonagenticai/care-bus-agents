-- MCP Tasks (spec docs/superpowers/specs/2026-10-30-mcp-tasks-client.md, C3-C6). A long-running MCP tool call (an
-- n8n workflow, an fr project) comes back from nonagenticai_mcp_server as a durable task. care-bus keeps the handle
-- here, the researcher's task poller (crate::mcp_tasks) polls it until it is terminal, staff answer its questions and
-- cancel it through `/api/jobs/{job}/mcp-tasks`, and the final result is stored on the row before the server drops
-- it (`ttlMs`). Grants come from 0007's default privileges.

CREATE TABLE mcp_tasks (
    tenant_id      uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id         uuid NOT NULL DEFAULT app_current_job(),
    -- The server's taskId (`<uuid>`); unique for the life of the task.
    task_id        text PRIMARY KEY CHECK (length(task_id) BETWEEN 1 AND 200),
    case_id        uuid,
    -- The research run whose tool call started the task, when one did.
    run_id         uuid,
    tool           text NOT NULL CHECK (length(tool) BETWEEN 1 AND 200),
    status         text NOT NULL DEFAULT 'working'
                   CHECK (status IN ('working', 'input_required', 'completed', 'cancelled', 'failed', 'lost')),
    status_message text,
    -- When the poller next asks the server; a claim pushes it forward (the lease), so a crashed poller's row is
    -- claimed again once that passes.
    poll_after     timestamptz NOT NULL DEFAULT now(),
    -- The last tasks/get answer (DetailedTask wire object: inputRequests while input_required).
    last_detail    jsonb,
    -- The terminal outcome (crate::mcp_tasks::TaskOutcome::to_json), written before the row is terminal.
    result         jsonb,
    cancel_requested boolean NOT NULL DEFAULT false,
    -- tasks/cancel was acknowledged; until then the poller retries it.
    cancel_sent    boolean NOT NULL DEFAULT false,
    created_by     text NOT NULL,
    created_at     timestamptz NOT NULL DEFAULT now(),
    updated_at     timestamptz NOT NULL DEFAULT now(),
    terminal_at    timestamptz,
    CONSTRAINT mcp_tasks_tenant_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id),
    CONSTRAINT mcp_tasks_run_fkey FOREIGN KEY (tenant_id, run_id) REFERENCES research_runs (tenant_id, id)
        ON DELETE SET NULL (run_id),
    CONSTRAINT mcp_tasks_tenant_task_key UNIQUE (tenant_id, task_id),
    CONSTRAINT mcp_tasks_terminal CHECK ((status IN ('completed', 'cancelled', 'failed', 'lost')) = (terminal_at IS NOT NULL))
);
CREATE INDEX mcp_tasks_due ON mcp_tasks (poll_after) WHERE status IN ('working', 'input_required');
CREATE INDEX mcp_tasks_job ON mcp_tasks (job_id, created_at DESC);

-- One staff answer per input request key: a double submit is a no-op. `sent_at` is set once tasks/update was
-- acknowledged; the key is never reused by the server.
CREATE TABLE mcp_task_answers (
    tenant_id   uuid NOT NULL DEFAULT app_current_tenant() REFERENCES tenants (id),
    job_id      uuid NOT NULL DEFAULT app_current_job(),
    task_id     text NOT NULL,
    input_key   text NOT NULL CHECK (length(input_key) BETWEEN 1 AND 300),
    -- The human who answered (care-bus subject); the server is told `care-bus:<tenant>:<answered_by>`.
    answered_by text NOT NULL,
    -- The ElicitResult sent: {"action", "content"?}.
    response    jsonb NOT NULL,
    sent_at     timestamptz,
    created_at  timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (task_id, input_key),
    CONSTRAINT mcp_task_answers_task_fkey FOREIGN KEY (tenant_id, task_id) REFERENCES mcp_tasks (tenant_id, task_id)
        ON DELETE CASCADE,
    CONSTRAINT mcp_task_answers_tenant_job_fkey FOREIGN KEY (tenant_id, job_id) REFERENCES jobs (tenant_id, id)
);

-- A run whose tool call started a task parks as 'waiting' (no lease, never claimed: carebus_research_claim takes only
-- queued or expired-lease running/verifying runs) until the task finishes and requeues it. The column's unnamed
-- 0020 CHECK is named research_runs_status_check by Postgres.
ALTER TABLE research_runs DROP CONSTRAINT research_runs_status_check;
ALTER TABLE research_runs ADD CONSTRAINT research_runs_status_check
    CHECK (status IN ('queued', 'running', 'waiting', 'verifying', 'done', 'failed', 'cancelled'));

-- RLS as 0007: tenant isolation + restrictive job scope.
DO $$
DECLARE
    t text;
BEGIN
    FOREACH t IN ARRAY ARRAY['mcp_tasks', 'mcp_task_answers'] LOOP
        EXECUTE format('ALTER TABLE %I ENABLE ROW LEVEL SECURITY', t);
        EXECUTE format('CREATE POLICY tenant_isolation ON %I FOR ALL '
                       'USING (tenant_id = app_current_tenant()) WITH CHECK (tenant_id = app_current_tenant())', t);
        EXECUTE format('CREATE POLICY job_scope ON %I AS RESTRICTIVE FOR ALL '
                       'USING (app_current_job() IS NULL OR job_id = app_current_job()) '
                       'WITH CHECK (app_current_job() IS NULL OR job_id = app_current_job())', t);
    END LOOP;
END $$;

-- The poller is not a tenant: this definer function is its one cross-tenant read+write. It leases up to p_limit due
-- tasks (non-terminal, poll_after passed) by pushing poll_after p_lease_s ahead, and returns only the scope needed to
-- continue under RLS. A poller that dies leaves the row to be claimed again when the lease passes.
CREATE FUNCTION carebus_mcp_task_claim(p_lease_s integer, p_limit integer)
    RETURNS TABLE (tenant_id uuid, job_id uuid, task_id text)
    LANGUAGE plpgsql VOLATILE SECURITY DEFINER
    SET search_path = public, pg_temp
    AS $$
    BEGIN
        IF p_lease_s IS NULL OR p_lease_s < 5 OR p_lease_s > 3600 OR p_limit IS NULL OR p_limit < 1 OR p_limit > 100 THEN
            RAISE EXCEPTION 'carebus_mcp_task_claim: a lease of 5..3600 s and a limit of 1..100 are required'
                USING ERRCODE = 'invalid_parameter_value';
        END IF;
        RETURN QUERY
        UPDATE mcp_tasks m
           SET poll_after = now() + make_interval(secs => p_lease_s)
         WHERE m.task_id IN (
                SELECT c.task_id FROM mcp_tasks c
                 WHERE c.status IN ('working', 'input_required') AND c.poll_after <= now()
                 ORDER BY c.poll_after
                 LIMIT p_limit
                 FOR UPDATE SKIP LOCKED)
        RETURNING m.tenant_id, m.job_id, m.task_id;
    END
    $$;

REVOKE ALL ON FUNCTION carebus_mcp_task_claim(integer, integer) FROM PUBLIC;
GRANT EXECUTE ON FUNCTION carebus_mcp_task_claim(integer, integer) TO carebus_app;
