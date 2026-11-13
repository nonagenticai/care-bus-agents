// Research runs (plan 18, `/api/jobs/{job}/research-runs`): a long-running, cited research or Polish legal research
// run, executed by the `researcher` worker. Types mirror crates/carebus-api src/routes/research.rs (RunView,
// RunSummary, StepView, ClaimView, CitationView). No carebus-api contract fixture exists for them yet: the inline
// samples in tests/unit/research.test.ts are held to RESEARCH_SHAPES. The pure helpers below are what the research
// page renders.
import { MATTERS_ENABLED } from '../features';
import { keyMap } from './shapes';

export type ResearchProfile = 'research' | 'legal';
export type RunStatus = 'queued' | 'running' | 'waiting' | 'verifying' | 'done' | 'failed' | 'cancelled';
export type StepKind = 'model' | 'tool';
export type StepStatus = 'ok' | 'error' | 'denied';
export type ClaimStatus = 'pending' | 'supported' | 'unsupported';
export type SourceKind =
	| 'strata'
	| 'web'
	| 'eli'
	| 'saos'
	| 'eurlex'
	| 'krs'
	| 'literature'
	| 'stats'
	| 'other';
export type VerifyStatus =
	| 'pending'
	| 'verified'
	| 'quote_mismatch'
	| 'source_unresolved'
	| 'not_in_force'
	| 'unverifiable';

export const PROFILES: readonly ResearchProfile[] = ['research', 'legal'];
export const PROFILE_LABEL: Record<ResearchProfile, string> = { research: 'Research', legal: 'Legal (PL)' };
export const RUN_STATUSES: readonly RunStatus[] = [
	'queued',
	'running',
	'waiting',
	'verifying',
	'done',
	'failed',
	'cancelled',
];
export const TERMINAL: readonly RunStatus[] = ['done', 'failed', 'cancelled'];
export const STATUS_LABEL: Record<RunStatus, string> = {
	queued: 'Queued',
	running: 'Running',
	waiting: 'Waiting for a task',
	verifying: 'Verifying',
	done: 'Done',
	failed: 'Failed',
	cancelled: 'Cancelled',
};
export const VERIFY_LABEL: Record<VerifyStatus, string> = {
	pending: 'Not checked yet',
	verified: 'Verified',
	quote_mismatch: 'Quote not in source',
	source_unresolved: 'Source not found',
	not_in_force: 'Not in force',
	unverifiable: 'Unverifiable',
};
/** The banner every legal run carries (spec plan 18; Naczelna Rada Adwokacka uchwała 41/2026). */
export const LEGAL_BANNER = 'Draft for lawyer review — AI output is auxiliary only (NRA uchwała 41/2026).';
/** Server limits (routes::research): question length and stores per run. */
export const QUESTION_MAX = 8000;
export const STORES_MAX = 20;
/** How often a run that is not finished is read again. */
export const RESEARCH_POLL_MS = 3000;

/** `{}` = the runner's defaults; only the keys given are stored. */
export interface ResearchBudget {
	max_steps?: number;
	max_tool_calls?: number;
	max_wall_s?: number;
	max_tokens?: number;
}
export type BudgetKey = keyof ResearchBudget;

/**
 * POST /api/jobs/{job}/research-runs. `store_ids` are strata store ids of this job's knowledge stores. `matter_id`
 * (a legal matter the caller is a member of) is required for `legal`; with it, every store must be attached to the
 * matter. A store of another matter, or one above it in the strata tree, is refused (422 store_walled).
 */
export interface ResearchRunInput {
	profile: ResearchProfile;
	question: string;
	store_ids?: string[];
	budget?: ResearchBudget;
	matter_id?: string;
}

/** routes::research `RunView`: POST answers it (201), cancel answers it, GET one carries it as `run`. */
export interface ResearchRun {
	id: string;
	profile: ResearchProfile;
	/** The legal matter the run is filed under; only its members see the run. */
	matter_id: string | null;
	question: string;
	store_ids: string[];
	status: RunStatus;
	budget: ResearchBudget;
	steps_used: number;
	tokens_used: number;
	/** Times a runner claimed the run; past the runner's maximum it fails with `too_many_attempts`. */
	attempts: number;
	/** The verified memo, markdown. */
	report: string | null;
	error: string | null;
	cancel_requested: boolean;
	created_by: string;
	created_at: string;
	started_at: string | null;
	finished_at: string | null;
	updated_at: string;
}

/** routes::research `RunSummary` (the list): the question is cut to its first 200 characters. */
export interface ResearchRunSummary {
	id: string;
	profile: ResearchProfile;
	matter_id: string | null;
	question: string;
	status: RunStatus;
	steps_used: number;
	tokens_used: number;
	created_at: string;
	finished_at: string | null;
}

/** One loop step; `request` / `response` only with `?include=steps_full`. */
export interface ResearchStep {
	ordinal: number;
	kind: StepKind;
	status: StepStatus;
	duration_ms: number | null;
	/** The tool's name for a `tool` step; null for a model turn. */
	tool: string | null;
	created_at: string;
	request?: unknown;
	response?: unknown;
}

export interface ResearchClaim {
	id: string;
	ordinal: number;
	text: string;
	status: ClaimStatus;
	citation_ids: string[];
}

export interface ResearchCitation {
	id: string;
	source_kind: SourceKind;
	locator: string;
	title: string | null;
	quote: string;
	step_ordinal: number | null;
	verify_status: VerifyStatus;
	verify_detail: unknown;
	verified_at: string | null;
	created_at: string;
}

/** GET /api/jobs/{job}/research-runs/{id}. */
export interface ResearchRunDetail {
	run: ResearchRun;
	steps: ResearchStep[];
	claims: ResearchClaim[];
	citations: ResearchCitation[];
}

const RUN = keyMap<ResearchRun>()({
	id: true,
	profile: true,
	matter_id: true,
	question: true,
	store_ids: true,
	status: true,
	budget: true,
	steps_used: true,
	tokens_used: true,
	attempts: true,
	report: true,
	error: true,
	cancel_requested: true,
	created_by: true,
	created_at: true,
	started_at: true,
	finished_at: true,
	updated_at: true,
});
const SUMMARY = keyMap<ResearchRunSummary>()({
	id: true,
	profile: true,
	matter_id: true,
	question: true,
	status: true,
	steps_used: true,
	tokens_used: true,
	created_at: true,
	finished_at: true,
});
const STEP = keyMap<ResearchStep>()({
	ordinal: true,
	kind: true,
	status: true,
	duration_ms: true,
	tool: true,
	created_at: true,
	request: 'optional-null',
	response: 'optional-null',
});
const CLAIM = keyMap<ResearchClaim>()({
	id: true,
	ordinal: true,
	text: true,
	status: true,
	citation_ids: true,
});
const CITATION = keyMap<ResearchCitation>()({
	id: true,
	source_kind: true,
	locator: true,
	title: true,
	quote: true,
	step_ordinal: true,
	verify_status: true,
	verify_detail: true,
	verified_at: true,
	created_at: true,
});
const DETAIL = keyMap<ResearchRunDetail>()({ run: true, steps: true, claims: true, citations: true });

/** Key maps the research fixtures are held to (tests/unit/research.test.ts). */
export const RESEARCH_SHAPES = { RUN, SUMMARY, STEP, CLAIM, CITATION, DETAIL } as const;

// ---- status ---------------------------------------------------------------------------------------------------

export function isTerminal(s: RunStatus): boolean {
	return TERMINAL.includes(s);
}

/** The word `Status` colours a run status by (done = ok, in progress = warn, failed = err, rest muted). */
export function statusWord(s: RunStatus): string {
	return s === 'done' ? 'ok' : s === 'verifying' || s === 'waiting' ? 'running' : s;
}

/** A citation's badge: verified is green; not yet checked or unverifiable amber; a failed check red. */
export function verifyVariant(s: VerifyStatus): 'ok' | 'warn' | 'err' {
	if (s === 'verified') return 'ok';
	if (s === 'pending' || s === 'unverifiable' || s === 'not_in_force') return 'warn';
	return 'err';
}

/** `verify_detail` as one tooltip line: a string as is, an object as `key: value` pairs, nothing as `''`. */
export function verifyDetailText(d: unknown): string {
	if (d == null) return '';
	if (typeof d === 'string') return d;
	if (typeof d !== 'object' || Array.isArray(d)) return JSON.stringify(d);
	return Object.entries(d as Record<string, unknown>)
		.map(([k, v]) => `${k}: ${typeof v === 'string' ? v : JSON.stringify(v)}`)
		.join('; ');
}

// ---- citations --------------------------------------------------------------------------------------------------

/** An http(s) URL as is, else null. */
function httpUrl(s: string): string | null {
	try {
		const u = new URL(s.trim());
		return u.protocol === 'https:' || u.protocol === 'http:' ? u.href : null;
	} catch {
		return null;
	}
}

/**
 * ISAP's page for an ELI id of the Dziennik Ustaw / Monitor Polski (`DU/2024/1061`, optionally under
 * `https://eli.gov.pl/eli/`). Only from 2012 on, when an act has a position and no issue number, is the ISAP id
 * `W{DU|MP}{year}{position padded to 7}`; older ids need the issue number the ELI does not carry, so they get no link.
 */
export function eliHref(locator: string): string | null {
	const m = /^(?:https?:\/\/eli\.gov\.pl\/eli\/|eli\/)?(DU|MP)\/(\d{4})\/(\d{1,7})$/iu.exec(locator.trim());
	if (!m) return null;
	const year = Number(m[2]);
	if (year < 2012) return null;
	return `https://isap.sejm.gov.pl/isap.nsf/DocDetails.xsp?id=W${m[1]!.toUpperCase()}${year}${m[3]!.padStart(7, '0')}`;
}

/** Where a citation's source can be opened, or null when its locator is not a link (strata chunks, KRS, other). */
export function citationHref(c: Pick<ResearchCitation, 'source_kind' | 'locator'>): string | null {
	const loc = c.locator.trim();
	switch (c.source_kind) {
		case 'web':
			return httpUrl(loc);
		case 'eli':
			return eliHref(loc) ?? (loc.startsWith('https://eli.gov.pl/') ? httpUrl(loc) : null);
		case 'saos':
			return /^\d+$/u.test(loc) ? `https://www.saos.org.pl/judgments/${loc}` : null;
		case 'eurlex':
			return /^[0-9A-Z()]+$/u.test(loc)
				? `https://eur-lex.europa.eu/legal-content/PL/TXT/?uri=CELEX:${encodeURIComponent(loc)}`
				: null;
		default:
			return null;
	}
}

/** Each claim with its citations, in the claim's order; an id with no citation row is skipped. */
export function claimsWithCitations(
	d: Pick<ResearchRunDetail, 'claims' | 'citations'>,
): { claim: ResearchClaim; citations: ResearchCitation[] }[] {
	const byId = new Map(d.citations.map((c) => [c.id, c]));
	return [...d.claims]
		.sort((a, b) => a.ordinal - b.ordinal)
		.map((claim) => ({
			claim,
			citations: claim.citation_ids.map((id) => byId.get(id)).filter((c): c is ResearchCitation => !!c),
		}));
}

// ---- the new-run form --------------------------------------------------------------------------------------------

/** Server ranges of each budget key (routes::research `budget_json`). */
export const BUDGET_RULES: Record<BudgetKey, { label: string; min: number; max: number }> = {
	max_steps: { label: 'Max steps', min: 1, max: 200 },
	max_tool_calls: { label: 'Max tool calls', min: 1, max: 400 },
	max_wall_s: { label: 'Max wall time (s)', min: 60, max: 21_600 },
	max_tokens: { label: 'Max tokens', min: 1000, max: 5_000_000 },
};
export const BUDGET_KEYS = Object.keys(BUDGET_RULES) as BudgetKey[];

export interface ResearchDraft {
	profile: ResearchProfile;
	question: string;
	store_ids: string[];
	/** '' = no matter (the picker exists only while MATTERS_ENABLED). */
	matter_id: string;
	/** Blank = the runner's default. */
	budget: Record<BudgetKey, string>;
}

export function emptyResearchDraft(): ResearchDraft {
	return {
		profile: 'research',
		question: '',
		store_ids: [],
		matter_id: '',
		budget: { max_steps: '', max_tool_calls: '', max_wall_s: '', max_tokens: '' },
	};
}

/** Field → message, as the server would refuse it; `{}` = sendable. */
export function validateResearchDraft(
	d: ResearchDraft,
): Partial<Record<'question' | 'store_ids' | 'matter_id' | BudgetKey, string>> {
	const out: Partial<Record<'question' | 'store_ids' | 'matter_id' | BudgetKey, string>> = {};
	const n = [...d.question.trim()].length;
	if (n === 0) out.question = 'Ask a question.';
	else if (n > QUESTION_MAX) out.question = `At most ${QUESTION_MAX} characters (${n} now).`;
	if (d.store_ids.length > STORES_MAX) out.store_ids = `At most ${STORES_MAX} stores.`;
	for (const k of BUDGET_KEYS) {
		const v = d.budget[k].trim();
		if (!v) continue;
		const r = BUDGET_RULES[k];
		if (!/^\d+$/u.test(v) || Number(v) < r.min || Number(v) > r.max)
			out[k] = `A whole number from ${r.min} to ${r.max}.`;
	}
	return out;
}

/** The POST body: the question trimmed, stores, matter and budget only when given. */
export function toResearchInput(d: ResearchDraft): ResearchRunInput {
	const input: ResearchRunInput = { profile: d.profile, question: d.question.trim() };
	if (d.store_ids.length) input.store_ids = [...d.store_ids];
	if (d.matter_id) input.matter_id = d.matter_id;
	const budget: ResearchBudget = {};
	for (const k of BUDGET_KEYS) {
		const v = d.budget[k].trim();
		if (v) budget[k] = Number(v);
	}
	if (Object.keys(budget).length) input.budget = budget;
	return input;
}

/** One line for the user from a create refusal's code; `null` = show the server's message. */
export function createProblem(code: string): string | null {
	switch (code) {
		case 'unknown_store':
			return 'One of the chosen stores is no longer a knowledge store of this job. Reload the stores and choose again.';
		case 'unknown_matter':
			return 'You are not a member of that matter.';
		case 'matter_closed':
			return 'That matter is closed.';
		case 'store_not_in_matter':
			return 'One of the chosen stores is not attached to the matter.';
		case 'store_walled':
			if (!MATTERS_ENABLED) return 'One of the chosen stores is restricted; choose other stores.';
			return 'One of the chosen stores belongs to a legal matter; file the run under that matter.';
		default:
			return null;
	}
}

/**
 * The stores a run may search: with a matter (`matterStores` = its attached store ids), only those; without one,
 * every store (the server refuses one walled by a matter the caller may not even see: 422 store_walled).
 */
export function offerableStores<S extends { strata_store_id: string }>(
	stores: S[],
	matterStores: readonly string[] | null,
): S[] {
	return matterStores ? stores.filter((s) => matterStores.includes(s.strata_store_id)) : stores;
}

/** The list row's question: the first line, cut to `max` characters. */
export function questionSnippet(q: string, max = 120): string {
	const line = q.trim().split(/\r?\n/u)[0] ?? '';
	return line.length > max ? `${line.slice(0, max - 1).trimEnd()}…` : line;
}

/** `2026-10-23 09:15:02`, or `—`. */
export function when(iso: string | null): string {
	return iso ? iso.replace('T', ' ').slice(0, 19) : '—';
}
