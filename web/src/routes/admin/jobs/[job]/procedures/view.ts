import type {
	ProcedureHistoryEntry,
	ProcedureStep,
	ProcedureStepInput,
	ProcedureSummary,
	VersionStatus,
} from '$lib/api/types';

// `plan` is typed `unknown` in the API types (orchestrator plan JSON); read it defensively.
export interface PlanStep {
	as: string | null;
	action: string | null;
	worker: string | null;
	args: Record<string, unknown>;
}
export interface PlanPhase {
	gate: string | null;
	steps: PlanStep[];
}

const obj = (v: unknown): Record<string, unknown> | null =>
	v && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null;
const str = (v: unknown): string | null => (typeof v === 'string' && v ? v : null);

export function planPhases(plan: unknown): PlanPhase[] {
	const phases = obj(plan)?.phases;
	if (!Array.isArray(phases)) return [];
	return phases.map((ph) => {
		const p = obj(ph);
		const steps = Array.isArray(p?.steps) ? p.steps : [];
		return {
			gate: str(p?.gate),
			steps: steps.map((s) => {
				const o = obj(s);
				return {
					as: str(o?.as),
					action: str(o?.action),
					worker: str(o?.worker),
					args: obj(o?.args) ?? {},
				};
			}),
		};
	});
}

// "n steps in k phases": the plan is what the orchestrator runs; fall back to the summary count.
export function stepSummary(p: Pick<ProcedureSummary, 'steps' | 'plan'>): { steps: number; phases: number } {
	const phases = planPhases(p.plan);
	if (!phases.length) return { steps: p.steps, phases: 0 };
	return { steps: phases.reduce((a, ph) => a + ph.steps.length, 0), phases: phases.length };
}

export interface FlowPhase {
	phase: number;
	label: string;
	steps: ProcedureStep[];
	// `as` alias of the gating step (e.g. "rules"); null when the phase has no gate.
	gate: string | null;
}

// Group detail steps into phases (ordered by phase, then step_number). A phase is gated when
// any of its steps has gate=true; the gate name comes from the plan, else the gating step's action.
export function groupPhases(steps: ProcedureStep[], plan?: unknown): FlowPhase[] {
	const plans = planPhases(plan);
	const order = [...new Set(steps.map((s) => s.phase))].sort((a, b) => a - b);
	return order.map((phase, i) => {
		const ps = steps.filter((s) => s.phase === phase).sort((a, b) => a.step_number - b.step_number);
		const gating = ps.find((s) => s.gate);
		const gate = gating ? (plans[i]?.gate ?? gating.action_type) : null;
		return { phase, label: `Phase ${i + 1}${ps.length > 1 ? ' · parallel' : ''}`, steps: ps, gate };
	});
}

const norm = (s: string) => s.replace(/[-_]/g, '').toLowerCase();

// Parameter bindings "?param ← $source.field": the step row names the plan arg, the plan step holds its source.
export function paramBindings(step: ProcedureStep, plan?: unknown): [string, string][] {
	const flat = planPhases(plan).flatMap((ph) => ph.steps);
	const match =
		flat.find((p) => p.action && norm(p.action) === norm(step.action_type) && p.worker === step.worker) ??
		flat.find((p) => p.action && norm(p.action) === norm(step.action_type));
	return Object.entries(step.parameters ?? {}).map(([k, v]) => {
		const arg = typeof v === 'string' ? v : JSON.stringify(v);
		const src = match?.args[arg];
		return [k, typeof src === 'string' ? src : src != null ? JSON.stringify(src) : arg];
	});
}

const STATUS_ORDER: VersionStatus[] = ['PUBLISHED', 'CANARY', 'DRAFT', 'ARCHIVED', 'DISCARDED'];
const LABEL: Record<VersionStatus, string> = {
	PUBLISHED: 'Published',
	CANARY: 'Canary',
	DRAFT: 'Draft',
	ARCHIVED: 'Archived',
	DISCARDED: 'Discarded',
};

// Segmented filter: All (n) plus one entry per status that occurs, with counts.
export function statusFilters(items: Pick<ProcedureSummary, 'version_status'>[]): [string, string][] {
	const out: [string, string][] = [['all', `All (${items.length})`]];
	for (const s of STATUS_ORDER) {
		const n = items.filter((p) => p.version_status === s).length;
		if (n) out.push([s, `${LABEL[s]} (${n})`]);
	}
	return out;
}

export const LIFECYCLE: VersionStatus[] = ['DRAFT', 'CANARY', 'PUBLISHED', 'ARCHIVED'];

const versionCmp = (a: string, b: string) => b.localeCompare(a, 'en', { numeric: true });

// All listed versions of one procedure, newest first.
export function versionHistory<T extends Pick<ProcedureSummary, 'name' | 'version' | 'updated_at'>>(
	items: T[],
	name: string,
): T[] {
	return items
		.filter((p) => p.name === name)
		.sort((a, b) => b.updated_at.localeCompare(a.updated_at) || versionCmp(a.version, b.version));
}

// One audit row of a version, in words.
export function historyText(h: ProcedureHistoryEntry): string {
	if (h.action === 'create') return 'Created as DRAFT';
	if (h.action === 'approve') return 'Approved';
	if (h.action !== 'transition') return h.action;
	if (!h.ok) return `${h.from ?? '?'} → ${h.to ?? '?'} refused${h.reason ? ` (${h.reason})` : ''}`;
	if (h.replaced_by) return `Archived: ${h.replaced_by} was published in its place`;
	if (h.from === 'ARCHIVED' && h.to === 'PUBLISHED') return 'Rolled back to: re-published from ARCHIVED';
	return `${h.from ?? '?'} → ${h.to ?? '?'}`;
}

// Next patch version for a "start from" copy: 1.3.0 -> 1.3.1, 1.4.0-beta -> 1.4.1.
export function nextVersion(v: string): string {
	const m = /^(\d+)\.(\d+)\.(\d+)/.exec(v);
	return m ? `${m[1]}.${m[2]}.${Number(m[3]) + 1}` : '';
}

// Step rows drafted from a plan, one per plan step in plan order (what routes::procedures::check_steps_mirror_plan
// requires). The MCP tool is the listed tool whose name ends in `_<action>` (order_get_order,
// rules_check_return_window); left empty when none matches, for the author to fill in.
export function draftSteps(plan: unknown, tools: string[] = []): ProcedureStepInput[] {
	let n = 0;
	return planPhases(plan).flatMap((ph, phase) =>
		ph.steps.map((st) => {
			const action = st.action ?? '';
			const worker = st.worker ?? '';
			const tool = action ? tools.find((t) => t.endsWith(`_${action}`)) : undefined;
			return {
				step_number: ++n,
				phase,
				name: action ? action.charAt(0).toUpperCase() + action.slice(1).replaceAll('_', ' ') : `Step ${n}`,
				action_type: action.replaceAll('_', '-'),
				worker,
				mcp_tool: tool ?? '',
				performed_by: worker ? `${worker} worker` : '',
				preconditions: [],
				postconditions: [],
				parameters: {},
				gate: ph.gate !== null && st.as === ph.gate,
			};
		}),
	);
}

// The stored rows of an existing version as create input (the detail adds nothing a step input lacks).
export const stepInputs = (steps: ProcedureStep[]): ProcedureStepInput[] =>
	steps.map(
		({
			step_number,
			phase,
			name,
			action_type,
			worker,
			mcp_tool,
			performed_by,
			preconditions,
			postconditions,
			parameters,
			gate,
		}) => ({
			step_number,
			phase,
			name,
			action_type,
			worker,
			mcp_tool,
			performed_by,
			preconditions,
			postconditions,
			parameters,
			gate,
		}),
	);

export type FormField = 'name' | 'version' | 'context_space' | 'plan' | 'steps' | null;
// Which form field a POST /api/procedures error code belongs under (routes::procedures::create).
export function errorField(code: string): FormField {
	switch (code) {
		case 'invalid_plan':
			return 'plan';
		case 'steps_do_not_match_plan':
		case 'invalid_steps':
			return 'steps';
		case 'invalid_version':
		case 'version_exists':
			return 'version';
		case 'unknown_context_space':
			return 'context_space';
		default:
			return null;
	}
}

// JSON text from a textarea: the value, or a message saying where it does not parse.
export function parseJson(text: string): { ok: true; value: unknown } | { ok: false; error: string } {
	try {
		return { ok: true, value: JSON.parse(text) };
	} catch (e) {
		return { ok: false, error: `Not valid JSON: ${(e as Error).message}` };
	}
}

/**
 * Success over the last 7 days, from GET /api/procedures: `success_rate` = (completed + not_eligible) / runs_7d --
 * a not-eligible case is the gate deciding "no", the procedure working as designed; only `failed` is a failure.
 * Null (no runs) shows "—", never 0 %. `cls` colours the meter: ok from 95 %, warn from 80 %, else err.
 */
export function successView(rate: number | null): { text: string; cls: string } | null {
	if (rate === null) return null;
	const cls = rate >= 0.95 ? 's-ok' : rate >= 0.8 ? 's-warn' : 's-err';
	return { text: `${Math.round(rate * 100)}%`, cls };
}
