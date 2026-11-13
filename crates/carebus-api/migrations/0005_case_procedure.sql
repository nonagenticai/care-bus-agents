-- The procedure version a case ran, from the `procedure` (`name@version`) the orchestrator stamps on its
-- `request.resolved` envelope. NULL for cases recorded before the stamp existed, cases that never got a plan (the
-- registry was down) and built-in plans: there is no way to tell which version an older case ran (a CANARY and the
-- PUBLISHED version of one context space can run side by side), so existing rows are never backfilled by guessing.
ALTER TABLE case_history
    ADD COLUMN procedure_name    text,
    ADD COLUMN procedure_version text;

-- GET /api/procedures: runs and outcomes per version over a recent window.
CREATE INDEX case_history_procedure ON case_history (procedure_name, procedure_version, started_at DESC)
    WHERE procedure_name IS NOT NULL;
