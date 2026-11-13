-- Plan 18: citations of the research_literature_* MCP tools (discovery-engine) and of research_stats_* results.
-- 'literature' (locator = discovery-engine paper id, optionally `doi:` / `pmid:`) is verified against the cited
-- paper's title/abstract in the named step; 'stats' (a computed number) is never verified: a computation is not a
-- source (research::verify).
ALTER TABLE research_citations DROP CONSTRAINT research_citations_source_kind_check;
ALTER TABLE research_citations ADD CONSTRAINT research_citations_source_kind_check
    CHECK (source_kind IN ('strata', 'web', 'eli', 'saos', 'eurlex', 'krs', 'literature', 'stats', 'other'));
