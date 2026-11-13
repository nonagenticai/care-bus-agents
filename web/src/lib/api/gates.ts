// Laya gates (spec 2026-10-18 §3): Jev System-1 questions asked at three points of a turn. Types mirror the admin
// routes carebus-api serves under `/api/jobs/{job}/gates`; the pure helpers below (rule summary, fire evaluation,
// criteria validation) are what the gates page renders and are unit-tested in tests/unit/gates.test.ts.
import { ACTIVE_TENANT_HEADER, ApiError } from './client';

export type GatePoint = 'request' | 'tool' | 'response';
export type GateType = 'noul' | 'choice' | 'score';
export type FireWhen = 'gte' | 'lt' | 'in';
export type GateAction = 'handoff' | 'block' | 'annotate';
export type FailPolicy = 'open' | 'closed';
export type GateOutcome = 'pass' | 'fired' | 'failed_open' | 'failed_closed' | 'skipped';
/** What a question reads: the customer's own words this turn, or the full request (message, type, details). */
export type GateInput = 'message' | 'request';

export const POINTS: GatePoint[] = ['request', 'tool', 'response'];
export const POINT_LABEL: Record<GatePoint, string> = {
	request: 'Request',
	tool: 'Tool',
	response: 'Response',
};
export const TYPES: GateType[] = ['noul', 'choice', 'score'];
export const ACTIONS: GateAction[] = ['handoff', 'block', 'annotate'];
export const INPUT_LABEL: Record<GateInput, string> = {
	message: 'Customer message only',
	request: 'Full request (message, type, details)',
};
/** carebus-api's limit on a precheck pattern. */
export const MAX_PRECHECK = 500;

/** noul: `{true, false}`; choice: option → description (2–255); score: ordered level labels (2–10). */
export type NoulCriteria = { true: string; false: string };
export type ChoiceCriteria = Record<string, string>;
export type ScoreCriteria = string[];
export type GateCriteria = NoulCriteria | ChoiceCriteria | ScoreCriteria;

export interface GateQuestion {
	key: string;
	point: GatePoint;
	type: GateType;
	instructions: string;
	criteria: GateCriteria;
	fire_when: FireWhen;
	threshold: number | null;
	fire_values: string[] | null;
	action: GateAction;
	message: string | null;
	fail: FailPolicy;
	tools: string[] | null;
	enabled: boolean;
	ordinal: number;
	/** Request point only may read `message`; absent (older servers): `request`. */
	input?: GateInput;
	/** A regex (Rust syntax) that must match the question's text before laya is asked; null = always asked. */
	precheck?: string | null;
	updated_at?: string | null;
	updated_by?: string | null;
}

/** PUT /api/jobs/{job}/gates/{key}: the question without its key (in the path) and audit columns. */
export type GateQuestionInput = Omit<GateQuestion, 'key' | 'updated_at' | 'updated_by'>;

export interface GatesView {
	questions: GateQuestion[];
	provider: { bound: boolean; model: string | null };
}

/** One answer as laya returned it: `noul` for noul, `choice`+`probabilities`+`confidence` for choice, `score`(+…) for score. */
export interface GateAnswer {
	noul?: number;
	choice?: string;
	score?: number;
	probabilities?: Record<string, number> | number[];
	confidence?: number;
	fired?: boolean;
	/** `precheck`: the question's precheck did not match its text, so laya was not asked (never fires). */
	skipped?: string;
}

export interface GateEvaluation {
	outcome: GateOutcome;
	action: GateAction | null;
	message: string | null;
	fired_keys: string[];
	answers: Record<string, GateAnswer> | null;
	latency_ms: number | null;
	error?: string | null;
	/** The point has an enabled fail-closed question (internal callers fail closed on a transport error). */
	has_closed?: boolean;
	/** Playground: per question, the text it was (or, skipped, would have been) asked about. */
	texts?: Record<string, string>;
}

export interface GateVerdict {
	id: number | string;
	point: GatePoint;
	conversation_id: string | null;
	case_id: string | null;
	outcome: GateOutcome;
	fired_keys: string[] | null;
	action: GateAction | null;
	answers?: Record<string, GateAnswer> | null;
	latency_ms: number | null;
	error: string | null;
	created_at: string;
}

export interface EvaluateInput {
	point: GatePoint;
	state: string;
	/** Tool point only: the tool the worker would call, so only questions covering it are asked. */
	tool?: string;
	/** Request point only: the draft the message belongs to, so the request text is built as a live turn builds it. */
	request?: PlaygroundRequest;
}

export interface PlaygroundRequest {
	type: string;
	/** Default: the message itself. */
	first_message?: string;
	details?: Record<string, unknown>;
	/** Complete request (`Details:`), else a draft (`Details so far:`). */
	complete?: boolean;
}

// ---------- pure helpers ----------

const OP: Record<FireWhen, string> = { gte: '≥', lt: '<', in: '∈' };

/** Score criteria as an ordered label list; tolerates a level-keyed object (`{"0":"calm",…}`). */
export function scoreLevels(c: GateCriteria | null | undefined): string[] {
	if (Array.isArray(c)) return c.map(String);
	if (c && typeof c === 'object') return Object.values(c).map(String);
	return [];
}

export function choiceOptions(c: GateCriteria | null | undefined): [string, string][] {
	if (!c || Array.isArray(c) || typeof c !== 'object') return [];
	return Object.entries(c).map(([k, v]) => [k, String(v)]);
}

/** A threshold as set: a finite number, else `null` (an emptied number input reads as null, undefined or NaN). */
export function setThreshold(t: number | null | undefined): number | null {
	return typeof t === 'number' && Number.isFinite(t) ? t : null;
}

/** `fires when noul ≥ 0.7 → handoff, fail-closed`; `fires when choice is yes with confidence ≥ 0.6 → block, …`. */
export function ruleSummary(
	q: Pick<GateQuestion, 'type' | 'fire_when' | 'threshold' | 'fire_values' | 'action' | 'fail' | 'criteria'>,
): string {
	let cond: string;
	if (q.fire_when === 'in') {
		const vals = q.fire_values ?? [];
		cond = vals.length === 1 ? `choice is ${vals[0]}` : `choice is one of {${vals.join(', ')}}`;
		const min = setThreshold(q.threshold);
		if (min != null) cond += ` with confidence ≥ ${min}`;
	} else {
		const subject = q.type === 'choice' ? 'confidence' : q.type;
		let t = q.threshold == null ? '?' : String(q.threshold);
		if (q.type === 'score' && q.threshold != null) {
			const label = scoreLevels(q.criteria)[q.threshold];
			if (label) t = `${q.threshold} (${label})`;
		}
		cond = `${subject} ${OP[q.fire_when]} ${t}`;
	}
	return `fires when ${cond} → ${q.action}, fail-${q.fail}`;
}

/** The value a question's threshold is compared with, from its answer (spec §3 Firing). */
export function answerValue(type: GateType, a: GateAnswer | null | undefined): number | null {
	if (!a) return null;
	const v = type === 'noul' ? a.noul : type === 'score' ? a.score : a.confidence;
	return typeof v === 'number' && Number.isFinite(v) ? v : null;
}

/**
 * Would this answer fire the question? Mirrors carebus-api: noul/score/choice-confidence against `threshold`
 * with `gte`/`lt`; `in` fires when the chosen option is in `fire_values` and, with a threshold, laya's confidence in
 * it is at least the threshold. `null` = cannot tell (no answer, or no confidence to compare).
 */
export function wouldFire(
	q: Pick<GateQuestion, 'type' | 'fire_when' | 'threshold' | 'fire_values'>,
	a: GateAnswer | null | undefined,
): boolean | null {
	if (!a) return null;
	if (q.fire_when === 'in') {
		if (typeof a.choice !== 'string') return null;
		if (!(q.fire_values ?? []).includes(a.choice)) return false;
		const min = setThreshold(q.threshold);
		if (min == null) return true;
		const c = answerValue('choice', a);
		return c == null ? null : c >= min;
	}
	const v = answerValue(q.type, a);
	if (v == null || q.threshold == null) return null;
	return q.fire_when === 'gte' ? v >= q.threshold : v < q.threshold;
}

/** Probabilities as `[label, p]` rows: an option map for choice, an index list labelled by level for score. */
export function probabilityRows(type: GateType, a: GateAnswer, criteria: GateCriteria): [string, number][] {
	const p = a.probabilities;
	if (!p) return [];
	if (Array.isArray(p)) {
		const labels = type === 'score' ? scoreLevels(criteria) : [];
		return p.map((v, i) => [labels[i] ? `${i} · ${labels[i]}` : String(i), Number(v)]);
	}
	return Object.entries(p).map(([k, v]) => [k, Number(v)]);
}

export type FieldErrors = Partial<
	Record<
		'key' | 'instructions' | 'criteria' | 'threshold' | 'fire_values' | 'tools' | 'input' | 'precheck',
		string
	>
>;

const KEY_RE = /^[a-z][a-z0-9_]{0,63}$/;

/** Criteria shape rules (laya 422s otherwise): noul exactly true/false, choice 2–255 options, score 2–10 levels. */
export function validateCriteria(type: GateType, c: unknown): string | null {
	if (type === 'noul') {
		if (!c || typeof c !== 'object' || Array.isArray(c))
			return 'A yes/no question needs a true and a false criterion.';
		const keys = Object.keys(c).sort();
		if (keys.join() !== 'false,true') return 'A yes/no question has exactly two criteria: true and false.';
		const n = c as Record<string, unknown>;
		if (typeof n.true !== 'string' || !n.true.trim() || typeof n.false !== 'string' || !n.false.trim())
			return 'Describe both the true and the false case.';
		return null;
	}
	if (type === 'choice') {
		if (!c || typeof c !== 'object' || Array.isArray(c)) return 'A choice question needs options.';
		const opts = Object.entries(c as Record<string, unknown>);
		if (opts.length < 2 || opts.length > 255)
			return `A choice question has 2–255 options (now ${opts.length}).`;
		if (opts.some(([k]) => !k.trim())) return 'Every option needs a name.';
		if (opts.some(([, v]) => typeof v !== 'string' || !v.trim())) return 'Every option needs a description.';
		return null;
	}
	if (!Array.isArray(c)) return 'A score question needs an ordered list of levels.';
	if (c.length < 2 || c.length > 10) return `A score question has 2–10 levels (now ${c.length}).`;
	if (c.some((l) => typeof l !== 'string' || !l.trim())) return 'Every level needs a label.';
	if (new Set(c.map((l: string) => l.trim())).size !== c.length) return 'Level labels must be distinct.';
	return null;
}

/** Field errors for a question about to be saved; empty when it may be sent. */
export function validateQuestion(q: GateQuestion, isNew: boolean, existing: string[] = []): FieldErrors {
	const e: FieldErrors = {};
	if (!KEY_RE.test(q.key)) e.key = 'Key: lowercase letters, digits and _, starting with a letter (max 64).';
	else if (isNew && existing.includes(q.key)) e.key = `A question named ${q.key} already exists.`;
	if (!q.instructions.trim()) e.instructions = 'Write the instructions laya answers.';
	const ce = validateCriteria(q.type, q.criteria);
	if (ce) e.criteria = ce;
	if (q.fire_when === 'in') {
		if (q.type !== 'choice') e.fire_values = '“in” only applies to choice questions.';
		else if (!q.fire_values?.length) e.fire_values = 'Pick at least one option that fires.';
		else {
			const opts = new Set(choiceOptions(q.criteria).map(([k]) => k));
			const unknown = q.fire_values.filter((v) => !opts.has(v));
			if (unknown.length) e.fire_values = `Not an option: ${unknown.join(', ')}.`;
		}
		const min = setThreshold(q.threshold);
		if (min != null && (min < 0 || min > 1))
			e.threshold = 'The minimum confidence is between 0 and 1 (blank: any).';
	} else if (q.threshold == null || !Number.isFinite(q.threshold)) {
		e.threshold = 'Set a threshold.';
	} else if (q.type === 'score') {
		const n = scoreLevels(q.criteria).length;
		if (!Number.isInteger(q.threshold) || q.threshold < 0 || q.threshold >= Math.max(n, 1))
			e.threshold = `A score threshold is a level index 0–${Math.max(n - 1, 0)}.`;
	} else if (q.threshold < 0 || q.threshold > 1) {
		e.threshold = 'The threshold is a probability between 0 and 1.';
	}
	if (q.point !== 'tool' && q.tools?.length) e.tools = 'Tools apply to tool-point questions only.';
	if (q.point !== 'request' && q.input === 'message')
		e.input = 'Only request-point questions can read the customer message alone.';
	// The pattern is Rust regex syntax (carebus-api compiles it on save); only its length is checked here.
	if ((q.precheck ?? '').trim().length > MAX_PRECHECK)
		e.precheck = `A precheck is at most ${MAX_PRECHECK} characters.`;
	return e;
}

/** Default criteria when a question's type changes (keeps what still fits). */
export function criteriaFor(type: GateType, prev?: GateCriteria): GateCriteria {
	if (type === 'noul') {
		const p = prev && !Array.isArray(prev) ? (prev as Partial<NoulCriteria>) : {};
		return {
			true: typeof p.true === 'string' ? p.true : '',
			false: typeof p.false === 'string' ? p.false : '',
		};
	}
	if (type === 'choice') {
		const o = choiceOptions(prev ?? {}).filter(([k]) => k !== 'true' && k !== 'false');
		return o.length >= 2 ? Object.fromEntries(o) : { option_a: '', option_b: '' };
	}
	const l = scoreLevels(prev).filter((x) => x.trim());
	return Array.isArray(prev) && l.length >= 2 ? l : ['low', 'high'];
}

export function newQuestion(point: GatePoint, ordinal: number): GateQuestion {
	return {
		key: '',
		point,
		type: 'noul',
		instructions: '',
		criteria: { true: '', false: '' },
		fire_when: 'gte',
		threshold: 0.7,
		fire_values: null,
		action: 'handoff',
		message: null,
		fail: 'closed',
		tools: point === 'tool' ? [] : null,
		enabled: true,
		ordinal,
		input: 'request',
		precheck: null,
	};
}

/**
 * The PUT body: normalised so a non-`in` rule sends no fire_values and a non-tool point no tools. An `in` rule sends
 * its optional minimum confidence (`null` = any confidence).
 */
export function toInput(q: GateQuestion): GateQuestionInput {
	const isIn = q.fire_when === 'in';
	return {
		point: q.point,
		type: q.type,
		instructions: q.instructions.trim(),
		criteria: Array.isArray(q.criteria) ? q.criteria.map((l) => l.trim()) : q.criteria,
		fire_when: q.fire_when,
		threshold: isIn ? setThreshold(q.threshold) : q.threshold,
		fire_values: isIn ? (q.fire_values ?? []) : null,
		action: q.action,
		message: q.message?.trim() ? q.message.trim() : null,
		fail: q.fail,
		tools: q.point === 'tool' ? (q.tools ?? []) : null,
		enabled: q.enabled,
		ordinal: q.ordinal,
		input: q.point === 'request' ? (q.input ?? 'request') : 'request',
		precheck: q.precheck?.trim() ? q.precheck.trim() : null,
	};
}

/** The answer of a question its precheck kept from laya. */
export function isSkipped(a: GateAnswer | null | undefined): boolean {
	return a?.skipped === 'precheck';
}

/** The keys of a verdict's precheck-skipped questions. */
export function skippedKeys(answers: Record<string, GateAnswer> | null | undefined): string[] {
	return Object.entries(answers ?? {})
		.filter(([, a]) => isSkipped(a))
		.map(([k]) => k);
}

/** Questions per point, by ordinal then key. */
export function byPoint(qs: GateQuestion[]): Record<GatePoint, GateQuestion[]> {
	const out: Record<GatePoint, GateQuestion[]> = { request: [], tool: [], response: [] };
	for (const q of qs) (out[q.point] ?? (out[q.point] = [])).push(q);
	for (const p of POINTS) out[p].sort((a, b) => a.ordinal - b.ordinal || a.key.localeCompare(b.key));
	return out;
}

/** Comma/newline separated tool names, trimmed and deduplicated. */
export function parseTools(s: string): string[] {
	return [
		...new Set(
			s
				.split(/[\s,]+/)
				.map((t) => t.trim())
				.filter(Boolean),
		),
	];
}

/**
 * Playground state for a point. Like the worker, the tool point sends only the customer's own words; the tool goes
 * separately. At the request point `text` is the customer's message; a request type (and its details) makes
 * carebus-api build the request text a live turn builds from a draft.
 */
export function playgroundState(
	point: GatePoint,
	text: string,
	tool = '',
	request?: { type: string; details?: Record<string, unknown> },
): EvaluateInput {
	if (point === 'request') {
		const type = request?.type.trim() ?? '';
		if (!type) return { point, state: text };
		return { point, state: text, request: { type, details: request?.details ?? {} } };
	}
	if (point !== 'tool') return { point, state: text };
	return { point, state: text, ...(tool.trim() ? { tool: tool.trim() } : {}) };
}

/** Request details typed as JSON: an object, else an error message. Blank = `{}`. */
export function parseDetails(s: string): Record<string, unknown> | string {
	if (!s.trim()) return {};
	try {
		const v: unknown = JSON.parse(s);
		if (v && typeof v === 'object' && !Array.isArray(v)) return v as Record<string, unknown>;
	} catch {
		/* not JSON */
	}
	return 'Details are a JSON object, e.g. {"order_id": "ORD-1042"}.';
}

export const OUTCOME_LABEL: Record<GateOutcome, string> = {
	pass: 'Pass',
	fired: 'Fired',
	failed_open: 'Failed open (passed)',
	failed_closed: 'Failed closed (handoff)',
	skipped: 'Skipped (gates off)',
};
export const OUTCOME_WORD: Record<GateOutcome, string> = {
	pass: 'ok',
	fired: 'warn',
	failed_open: 'degraded',
	failed_closed: 'failed',
	skipped: 'idle',
};

/** A verdict list as `[]`, `{items}` or `{verdicts}`. */
export function verdictItems(body: unknown): GateVerdict[] {
	if (Array.isArray(body)) return body as GateVerdict[];
	const b = body as { items?: unknown; verdicts?: unknown } | null;
	if (Array.isArray(b?.items)) return b.items as GateVerdict[];
	if (Array.isArray(b?.verdicts)) return b.verdicts as GateVerdict[];
	return [];
}

// ---------- requests ----------

type Token = (opts?: { force?: boolean }) => Promise<string | null>;
export type GatesOpts = {
	baseUrl: string;
	token: Token;
	job: () => string;
	activeTenant?: () => string | null;
	fetch?: typeof fetch;
};

/** The gates routes, with client.ts's request semantics: one forced-token retry on 401, `{error:{code,message}}`. */
export function createGatesApi(o: GatesOpts) {
	const f = o.fetch ?? ((...a: Parameters<typeof fetch>) => fetch(...a));
	const seg = encodeURIComponent;
	const J = (path: string) => `/api/jobs/${seg(o.job())}/gates${path}`;
	async function send(method: string, path: string, body: unknown, force: boolean): Promise<Response> {
		const token = await (force ? o.token({ force: true }) : o.token());
		const headers: Record<string, string> = { Accept: 'application/json' };
		const t = o.activeTenant?.();
		if (t) headers[ACTIVE_TENANT_HEADER] = t;
		if (token) headers.Authorization = `Bearer ${token}`;
		if (body !== undefined) headers['Content-Type'] = 'application/json';
		try {
			return await f(`${o.baseUrl}${path}`, {
				method,
				headers,
				body: body === undefined ? undefined : JSON.stringify(body),
			});
		} catch (e) {
			throw new ApiError(0, 'unavailable', `carebus-api is unreachable (${(e as Error).message})`);
		}
	}
	async function req<R>(method: string, path: string, body?: unknown): Promise<R> {
		let res = await send(method, path, body, false);
		if (res.status === 401) res = await send(method, path, body, true);
		if (res.status === 204) return undefined as R;
		const text = await res.text().catch(() => '');
		let data: unknown = null;
		let parsed = text === '';
		if (text !== '') {
			try {
				data = JSON.parse(text);
				parsed = true;
			} catch {
				/* not JSON */
			}
		}
		if (res.ok && !parsed)
			throw new ApiError(res.status, 'bad_response', 'carebus-api returned a non-JSON response');
		if (!res.ok) {
			const err = (data as { error?: { code?: string; message?: string } } | null)?.error;
			throw new ApiError(res.status, err?.code ?? `http_${res.status}`, err?.message ?? res.statusText);
		}
		return (text === '' ? undefined : data) as R;
	}
	return {
		list: () => req<GatesView>('GET', J('')),
		save: (key: string, q: GateQuestionInput) => req<GateQuestion>('PUT', J(`/${seg(key)}`), q),
		remove: (key: string) => req<void>('DELETE', J(`/${seg(key)}`)),
		evaluate: (input: EvaluateInput) => req<GateEvaluation>('POST', J('/evaluate'), input),
		verdicts: async (limit = 50) => verdictItems(await req<unknown>('GET', J(`/verdicts?limit=${limit}`))),
	};
}

export type GatesApi = ReturnType<typeof createGatesApi>;
