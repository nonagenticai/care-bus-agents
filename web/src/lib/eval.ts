// RAG eval page logic (routes::knowledge_eval over strata_rag /api/eval): question forms, per-question rank grids
// and the comparison of two runs. Pure functions, unit-tested in tests/unit/eval.test.ts.
import type {
	EvalMode,
	EvalModeReport,
	EvalProgress,
	EvalQuestion,
	EvalQuestionInput,
	EvalResult,
	EvalRun,
	EvalRunQuestion,
	EvalRunSummary,
} from './api/types';

export const EVAL_MODES: readonly EvalMode[] = ['embed', 'laya', 'hybrid', 'flat'];
export const MODE_LABEL: Record<EvalMode, string> = {
	embed: 'Embedding',
	laya: 'Laya routing',
	hybrid: 'Hybrid',
	flat: 'Flat (no routing)',
};
/** strata_rag's cap on a tenant's questions. */
export const EVAL_MAX_QUESTIONS = 200;

/** The add/edit form: ids as free text (one per line, or separated by `;`, `,` or spaces). */
export interface QuestionForm {
	query: string;
	expected_document: string;
	expected_leaf: string;
	expected_text: string;
	expected_document_ids: string;
	expected_chunk_ids: string;
}

export const emptyForm = (): QuestionForm => ({
	query: '',
	expected_document: '',
	expected_leaf: '',
	expected_text: '',
	expected_document_ids: '',
	expected_chunk_ids: '',
});

export function parseIds(text: string): string[] {
	return [...new Set(text.split(/[\s;,|]+/u).filter(Boolean))];
}

export function formOf(q: EvalQuestion): QuestionForm {
	return {
		query: q.query,
		expected_document: q.expected_document ?? '',
		expected_leaf: q.expected_leaf ?? '',
		expected_text: q.expected_text ?? '',
		expected_document_ids: q.expected_document_ids.join('\n'),
		expected_chunk_ids: q.expected_chunk_ids.join('\n'),
	};
}

/** The full question: blank optional text is `null` (so an edit clears it), ids parsed. */
export function inputOf(f: QuestionForm): Required<EvalQuestionInput> {
	const text = (v: string) => v.trim() || null;
	return {
		query: f.query.trim(),
		expected_document: text(f.expected_document),
		expected_leaf: text(f.expected_leaf),
		expected_text: text(f.expected_text),
		expected_document_ids: parseIds(f.expected_document_ids),
		expected_chunk_ids: parseIds(f.expected_chunk_ids),
	};
}

/** Why the form cannot be saved yet, or null. strata_rag checks the same (and more) and has the last word. */
export function formProblem(f: QuestionForm): string | null {
	const i = inputOf(f);
	if (!i.query) return 'Write the question.';
	if (
		!i.expected_document &&
		!i.expected_text &&
		i.expected_document_ids.length === 0 &&
		i.expected_chunk_ids.length === 0
	)
		return 'Give at least one expectation: a document, a text, document ids or chunk ids.';
	return null;
}

/** What counts as a hit, in words (any one of these). */
export function expectations(q: EvalRunQuestion): string[] {
	const out: string[] = [];
	if (q.expected_document) out.push(`document “${q.expected_document}”`);
	if (q.expected_text) out.push(`text “${q.expected_text}”`);
	const n = (k: number, what: string) => `${k} ${what}${k === 1 ? '' : 's'}`;
	if (q.expected_document_ids.length) out.push(n(q.expected_document_ids.length, 'document id'));
	if (q.expected_chunk_ids.length) out.push(n(q.expected_chunk_ids.length, 'chunk id'));
	return out;
}

export const rankLabel = (rank: number | null | undefined) =>
	rank == null ? 'miss' : rank === 1 ? 'hit #1' : `#${rank}`;
export const pct = (v: number | null | undefined) => (v == null ? '—' : `${Math.round(v * 100)}%`);

export function reportOf(run: EvalRun, mode: EvalMode): EvalModeReport | undefined {
	return run.reports?.find((r) => r.mode === mode);
}

/** How often the page polls a running run (strata_rag scores it in the background). */
export const EVAL_POLL_MS = 2000;

/** `12 / 40 searches`, or '' when the run reports no progress. */
export function progressLabel(p: EvalProgress | null | undefined): string {
	return p ? `${p.done} / ${p.total} searches` : '';
}

/** The run's state as one chip: running (with progress), failed, or its verdict. */
export function runChip(r: Pick<EvalRunSummary, 'status' | 'verdict'> & { progress?: EvalProgress | null }): {
	text: string;
	variant: 'work' | 'ok' | 'err' | 'muted';
} {
	if (r.status === 'running')
		return {
			text: r.progress ? `running ${r.progress.done}/${r.progress.total}` : 'running',
			variant: 'work',
		};
	if (r.status === 'failed') return { text: 'failed', variant: 'err' };
	if (r.verdict === 'PASS') return { text: 'PASS', variant: 'ok' };
	if (r.verdict === 'FAIL') return { text: 'FAIL', variant: 'err' };
	return { text: r.status, variant: 'muted' };
}

/** A listed run brought up to date from a fresh read of it (keeps the summary's shape: no questions or results). */
export function withRun(listed: EvalRunSummary, read: EvalRun): EvalRunSummary {
	const { questions, results: _results, ...head } = read;
	return { ...listed, ...head, question_count: questions.length };
}

export interface GridRow {
	index: number;
	question: EvalRunQuestion;
	/** Per mode of the run: its result (absent when the run has no row for it). */
	cells: Partial<Record<EvalMode, EvalResult>>;
}

/** One row per evaluated question, in run order, with its result in each mode. */
export function grid(run: EvalRun): GridRow[] {
	const rows: GridRow[] = run.questions.map((question, index) => ({ index, question, cells: {} }));
	for (const r of run.results) {
		const row = rows[r.index];
		if (row) row.cells[r.mode] = r;
	}
	return rows;
}

export type Change = 'better' | 'worse' | 'same';

/** How a question moved from run A to run B in one mode. A lower rank is better; a miss is worse than any hit. */
export function change(a: number | null | undefined, b: number | null | undefined): Change {
	const v = (r: number | null | undefined) => (r == null ? Infinity : r);
	if (v(a) === v(b)) return 'same';
	return v(b) < v(a) ? 'better' : 'worse';
}

export interface RecallDelta {
	mode: EvalMode;
	a: EvalModeReport;
	b: EvalModeReport;
	d1: number;
	d5: number;
	d10: number;
}

export interface CompareRow {
	query: string;
	/** Per mode both runs scored. */
	cells: Partial<Record<EvalMode, { a: number | null; b: number | null; change: Change }>>;
	/** The question is in one run only. */
	only?: 'a' | 'b';
}

export interface Comparison {
	modes: EvalMode[];
	recall: RecallDelta[];
	rows: CompareRow[];
	better: number;
	worse: number;
}

/** A question's identity across runs: its stored id, else its query (case-insensitive, as strata_rag's key). */
const keyOf = (q: EvalRunQuestion) => q.id ?? `q:${q.query.trim().toLowerCase()}`;

/**
 * Run B against run A (both with `results`): recall@k deltas per mode both scored, and per question the rank in
 * each such mode. Questions are matched by stored id, so an edited query still lines up with its earlier self.
 */
export function compare(a: EvalRun, b: EvalRun): Comparison {
	const modes = a.modes.filter((m) => b.modes.includes(m));
	const recall: RecallDelta[] = [];
	for (const mode of modes) {
		const ra = reportOf(a, mode);
		const rb = reportOf(b, mode);
		if (!ra || !rb) continue;
		recall.push({
			mode,
			a: ra,
			b: rb,
			d1: rb.recall_at_1 - ra.recall_at_1,
			d5: rb.recall_at_5 - ra.recall_at_5,
			d10: rb.recall_at_10 - ra.recall_at_10,
		});
	}
	const ga = new Map(grid(a).map((r) => [keyOf(r.question), r] as const));
	const gb = new Map(grid(b).map((r) => [keyOf(r.question), r] as const));
	const keys = [...ga.keys(), ...[...gb.keys()].filter((k) => !ga.has(k))];
	let better = 0;
	let worse = 0;
	const rows: CompareRow[] = keys.map((k) => {
		const ra = ga.get(k);
		const rb = gb.get(k);
		const query = (rb ?? ra)!.question.query;
		if (!ra || !rb) return { query, cells: {}, only: ra ? 'a' : 'b' };
		const cells: CompareRow['cells'] = {};
		for (const mode of modes) {
			const x = ra.cells[mode]?.rank ?? null;
			const y = rb.cells[mode]?.rank ?? null;
			const c = change(x, y);
			if (c === 'better') better++;
			if (c === 'worse') worse++;
			cells[mode] = { a: x, b: y, change: c };
		}
		return { query, cells };
	});
	return { modes, recall, rows, better, worse };
}

/** `+12 pts` / `−5 pts` / `±0`: a recall change in percentage points. */
export function delta(d: number): string {
	const pts = Math.round(d * 100);
	if (pts === 0) return '±0';
	return pts > 0 ? `+${pts} pts` : `−${-pts} pts`;
}
