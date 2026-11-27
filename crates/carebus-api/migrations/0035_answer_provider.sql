-- 0035: the website answer stage's provider use. The `faq` worker resolves the tenant's `answer`
-- binding (else the deployment's platform default) through POST /api/internal/answer-context, so
-- the model that composes the answer is configuration, not a worker env var.

ALTER TABLE llm_provider_uses DROP CONSTRAINT llm_provider_uses_use_check;
ALTER TABLE llm_provider_uses ADD CONSTRAINT llm_provider_uses_use_check
    CHECK ("use" IN ('qualify', 'gates', 'research', 'research_write', 'answer'));
