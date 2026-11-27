-- 0037: the website answer stage is retired. Owner ruling 2026-11-27: strata_rag `/api/ask` is the
-- only answer engine, and the `faq` worker sends the job's persona and budget to it. The `answer`
-- provider use has no reader left; delete its bindings and refuse the value from now on.
-- 0035 is already applied (sqlx checksums it); never edit it.

DELETE FROM llm_provider_uses WHERE "use" = 'answer';

ALTER TABLE llm_provider_uses DROP CONSTRAINT llm_provider_uses_use_check;
ALTER TABLE llm_provider_uses ADD CONSTRAINT llm_provider_uses_use_check
    CHECK ("use" IN ('qualify', 'gates', 'research', 'research_write'));
