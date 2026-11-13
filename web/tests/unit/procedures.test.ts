import { describe, expect, it } from 'vitest';
import type {
	ProcedureDetail,
	ProcedureHistoryEntry,
	ProcedureStep,
	ProcedureSummary,
} from '../../src/lib/api/types';
import {
	draftSteps,
	errorField,
	historyText,
	nextVersion,
	parseJson,
	stepInputs,
	groupPhases,
	paramBindings,
	planPhases,
	statusFilters,
	stepSummary,
	successView,
	versionHistory,
} from '../../src/routes/admin/jobs/[job]/procedures/view';
import detailFx from '../../src/lib/api/fixtures/procedure_version.json';
import listFx from '../../src/lib/api/fixtures/procedures.json';

const detail = detailFx as unknown as ProcedureDetail;
const list = listFx.items as unknown as ProcedureSummary[];

const step = (o: Partial<ProcedureStep>): ProcedureStep => ({
	step_number: 1,
	phase: 0,
	name: 's',
	action_type: 'a',
	worker: 'w',
	mcp_tool: 't',
	performed_by: 'w worker',
	preconditions: [],
	postconditions: [],
	parameters: {},
	gate: false,
	...o,
});

describe('planPhases', () => {
	it('reads the plan fixture', () => {
		const ph = planPhases(detail.plan);
		expect(ph.map((p) => p.gate)).toEqual([null, 'rules', null]);
		expect(ph[0]!.steps.map((s) => s.action)).toEqual(['verify_customer', 'get_order']);
	});
	it('tolerates malformed plans', () => {
		expect(planPhases(null)).toEqual([]);
		expect(planPhases({ phases: 'x' })).toEqual([]);
		expect(planPhases({ phases: [{ steps: [42] }] })).toEqual([
			{ gate: null, steps: [{ as: null, action: null, worker: null, args: {} }] },
		]);
	});
});

describe('groupPhases', () => {
	it('groups the fixture into labelled phases with the gate on the gated phase', () => {
		const g = groupPhases(detail.steps, detail.plan);
		expect(g.map((p) => p.label)).toEqual(['Phase 1 · parallel', 'Phase 2 · parallel', 'Phase 3']);
		expect(g.map((p) => p.steps.map((s) => s.step_number))).toEqual([[1, 2], [3, 4], [5]]);
		expect(g.map((p) => p.gate)).toEqual([null, 'rules', null]);
	});
	it('orders by phase then step_number and falls back to the gating action without a plan', () => {
		const g = groupPhases([
			step({ step_number: 3, phase: 5 }),
			step({ step_number: 2, phase: 1, gate: true, action_type: 'check-x' }),
			step({ step_number: 1, phase: 1 }),
		]);
		expect(g.map((p) => [p.phase, p.steps.map((s) => s.step_number), p.gate])).toEqual([
			[1, [1, 2], 'check-x'],
			[5, [3], null],
		]);
		expect(g[1]!.label).toBe('Phase 2');
	});
	it('returns nothing for no steps', () => {
		expect(groupPhases([], detail.plan)).toEqual([]);
	});
});

describe('stepSummary', () => {
	it('counts steps and phases from the plan', () => {
		expect(stepSummary(list[1]!)).toEqual({ steps: 5, phases: 3 });
		expect(stepSummary(list[2]!)).toEqual({ steps: 1, phases: 1 });
	});
	it('falls back to the summary count without a plan', () => {
		expect(stepSummary({ steps: 4, plan: null })).toEqual({ steps: 4, phases: 0 });
	});
});

describe('paramBindings', () => {
	it('resolves each parameter to its plan arg source', () => {
		const rules = detail.steps.find((s) => s.step_number === 3)!;
		expect(paramBindings(rules, detail.plan)).toEqual([
			['?category', '$order.category'],
			['?order_id', '$order.order_id'],
			['?delivered_on', '$order.delivered_on'],
		]);
	});
	it('falls back to the arg name when the plan has no matching step', () => {
		expect(paramBindings(step({ parameters: { '?x': 'x' } }), null)).toEqual([['?x', 'x']]);
	});
});

describe('statusFilters', () => {
	it('lists All plus only the statuses that occur, with counts', () => {
		expect(statusFilters(list)).toEqual([
			['all', 'All (6)'],
			['PUBLISHED', 'Published (5)'],
			['CANARY', 'Canary (1)'],
		]);
		expect(statusFilters([])).toEqual([['all', 'All (0)']]);
	});
});

describe('versionHistory', () => {
	it('returns the versions of one procedure, newest first', () => {
		expect(versionHistory(list, 'return').map((p) => p.version)).toEqual(['1.4.0-beta', '1.3.0']);
		expect(versionHistory(list, 'nope')).toEqual([]);
	});
	it('orders by updated_at before version', () => {
		const rows = [
			{ name: 'a', version: '2.0.0', updated_at: '2026-01-01' },
			{ name: 'a', version: '1.0.0', updated_at: '2026-02-01' },
		];
		expect(versionHistory(rows, 'a').map((p) => p.version)).toEqual(['1.0.0', '2.0.0']);
	});
});

describe('version history lines', () => {
	const h = (o: Partial<ProcedureHistoryEntry>): ProcedureHistoryEntry => ({
		at: '2026-10-16T09:00:00+00:00',
		actor: 'a.bakker',
		action: 'transition',
		ok: true,
		from: null,
		to: null,
		reason: null,
		replaced_by: null,
		...o,
	});
	it('says what each audit row did', () => {
		expect(historyText(h({ action: 'create' }))).toBe('Created as DRAFT');
		expect(historyText(h({ from: 'DRAFT', to: 'CANARY' }))).toBe('DRAFT → CANARY');
		expect(historyText(h({ action: 'approve' }))).toBe('Approved');
		expect(historyText(h({ from: 'PUBLISHED', to: 'ARCHIVED', replaced_by: '1.4.0-beta' }))).toBe(
			'Archived: 1.4.0-beta was published in its place',
		);
		expect(historyText(h({ from: 'ARCHIVED', to: 'PUBLISHED' }))).toBe(
			'Rolled back to: re-published from ARCHIVED',
		);
		expect(historyText(h({ ok: false, from: 'DRAFT', to: 'CANARY', reason: 'x' }))).toBe(
			'DRAFT → CANARY refused (x)',
		);
	});
});

describe('new procedure form', () => {
	it('drafts one step row per plan step, in the phase/worker order the API requires', () => {
		const tools = ['crm_verify_customer', 'order_get_order', 'rules_check_return_window', 'order_create_rma'];
		const rows = draftSteps(detail.plan, tools);
		// the same (phase, worker) sequence the stored step rows of the fixture have
		expect(rows.map((r) => [r.phase, r.worker])).toEqual(
			[...detail.steps].sort((a, b) => a.step_number - b.step_number).map((s) => [s.phase, s.worker]),
		);
		expect(rows.map((r) => r.step_number)).toEqual([1, 2, 3, 4, 5]);
		expect(rows[2]).toMatchObject({
			action_type: 'check-return-window',
			mcp_tool: 'rules_check_return_window',
			performed_by: 'business-rules worker',
			gate: true,
		});
		expect(rows[3]!.mcp_tool).toBe('');
		expect(rows.filter((r) => r.gate)).toHaveLength(1);
		expect(draftSteps(null)).toEqual([]);
	});
	it('copies stored steps as create input and bumps the patch version', () => {
		expect(stepInputs(detail.steps)).toEqual(detail.steps);
		expect(nextVersion('1.3.0')).toBe('1.3.1');
		expect(nextVersion('1.4.0-beta')).toBe('1.4.1');
		expect(nextVersion('x')).toBe('');
	});
	it('puts API errors under their field and reports bad JSON', () => {
		expect(errorField('steps_do_not_match_plan')).toBe('steps');
		expect(errorField('invalid_plan')).toBe('plan');
		expect(errorField('version_exists')).toBe('version');
		expect(errorField('unknown_context_space')).toBe('context_space');
		expect(errorField('secret_value_rejected')).toBeNull();
		expect(parseJson('{"a":1}')).toEqual({ ok: true, value: { a: 1 } });
		const bad = parseJson('{');
		expect(bad.ok).toBe(false);
	});
});

describe('successView', () => {
	it('is null (shown as —) when the version had no runs, never 0 %', () => {
		expect(successView(null)).toBeNull();
	});
	it('rounds to a percentage and colours by band', () => {
		expect(successView(1)).toEqual({ text: '100%', cls: 's-ok' });
		expect(successView(0.95)).toEqual({ text: '95%', cls: 's-ok' });
		expect(successView(0.8)).toEqual({ text: '80%', cls: 's-warn' });
		expect(successView(0.756)).toEqual({ text: '76%', cls: 's-err' });
		expect(successView(0)).toEqual({ text: '0%', cls: 's-err' });
	});
});
