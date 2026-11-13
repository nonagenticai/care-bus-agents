import { LLM_USES, type LlmUse } from './api/providers';

/**
 * Research runs (plan 18): the Research menu entry and page, the research LLM uses and the external-LLM setting.
 * Shown (owner, 2026-10-25). Set `false` to hide them: the entry goes, the page redirects to the job dashboard, and
 * the provider settings show only the chat uses. The API, the researcher and their data are unchanged either way.
 */
export const RESEARCH_ENABLED = true;

/**
 * Legal matters (plan 18, migration 0026). Hidden (owner, 2026-10-25: no value for single-member tenants): no menu
 * entry, `/matters` redirects to the job dashboard, and the research page shows no matter anywhere. The API still
 * enforces the walls when a run names a matter; a legal-profile run no longer needs one. Set `true` to show them.
 */
export const MATTERS_ENABLED = false;

/** The LLM uses the provider settings show: the research uses only while research is shown. */
export const SHOWN_LLM_USES: readonly LlmUse[] = LLM_USES.filter(
	(u) => RESEARCH_ENABLED || (u !== 'research' && u !== 'research_write'),
);
