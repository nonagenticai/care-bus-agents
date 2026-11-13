import { describe, expect, it } from 'vitest';
import {
	analyticsKpis,
	kpiDelta,
	overviewKpis,
	toolBudgetMs,
	toSeries,
} from '../../src/routes/admin/jobs/[job]/analytics/series';
import type { Overview } from '../../src/lib/api/types';

const kv = (value: number | null, previous: number | null) => ({
	label: 'x',
	value,
	previous,
	unit: 'n' as const,
	unavailable: '',
});

// Inline inputs (F7): the values asserted below are written here, not read from src/lib/api/fixtures.
const overview: Overview = {
	today: { cases: 312, completed: 276, not_eligible: 19, failed: 17, open: 0, median_ms: 4200 },
	week_ago: { cases: 300, completed: 270, not_eligible: 20, failed: 10, open: 0, median_ms: 3900 },
	health: [],
	attention: [],
};

describe('analytics helpers', () => {
	it('kpiDelta is null when a side is missing or the previous value is 0', () => {
		expect(kpiDelta(kv(120, 100))).toBeCloseTo(0.2);
		expect(kpiDelta(kv(120, null))).toBeNull();
		expect(kpiDelta(kv(null, 100))).toBeNull();
		expect(kpiDelta(kv(5, 0))).toBeNull();
	});
	it('overviewKpis derives tiles from today and week_ago', () => {
		const k = overviewKpis(overview);
		expect(k.map((x) => x.label)).toEqual([
			'Cases today',
			'Resolved by agents',
			'Not eligible',
			'Handed to a person',
			'Median time to answer',
		]);
		expect(k[0]).toMatchObject({ value: 312, previous: 300 });
		expect(k[1]!.value).toBeCloseTo(276 / 312);
		expect(k[3]).toMatchObject({ value: 17, previous: 10, good: 'down' });
		expect(k[4]).toMatchObject({ value: 4200, previous: 3900, good: 'down' });
	});
	it('a day with no cases shows rates as unavailable, not 0 %', () => {
		const zero = { cases: 0, completed: 0, not_eligible: 0, failed: 0, open: 0, median_ms: null };
		const k = overviewKpis({ today: zero, week_ago: zero, health: [], attention: [] });
		expect(k[1]).toMatchObject({ value: null, unavailable: 'no cases yet' });
		expect(k[2]).toMatchObject({ value: null, unavailable: 'no cases yet' });
		expect(k[4]).toMatchObject({ value: null, unavailable: 'no resolved cases yet' });
	});
	it('toSeries keeps the outcome order and colours', () => {
		const s = toSeries({
			days: [
				{ day: '2026-10-14', completed: 3, not_eligible: 1, failed: 0, open: 0 },
				{ day: '2026-10-15', completed: 4, not_eligible: 0, failed: 1, open: 2 },
			],
			by_type: [],
			latency: [],
			previous: { cases: 0, completed: 0, not_eligible: 0, failed: 0 },
		});
		expect(s.map((x) => [x.key, x.cls, x.values])).toEqual([
			['completed', 's-ok', [3, 4]],
			['not_eligible', 's-warn', [1, 0]],
			['failed', 's-err', [0, 1]],
		]);
	});
	it('analyticsKpis sums the period and compares it with previous', () => {
		const k = analyticsKpis({
			days: [
				{ day: '2026-10-14', completed: 6, not_eligible: 2, failed: 1, open: 0 },
				{ day: '2026-10-15', completed: 2, not_eligible: 0, failed: 1, open: 2 },
			],
			by_type: [
				{
					request_type: 'return',
					cases: 9,
					completed: 6,
					not_eligible: 2,
					failed: 1,
					retries: 3,
					p50_ms: 900,
				},
				{ request_type: 'faq', cases: 3, completed: 2, not_eligible: 0, failed: 1, retries: 1, p50_ms: null },
			],
			latency: [],
			previous: { cases: 10, completed: 5, not_eligible: 1, failed: 4 },
		});
		expect(k.map((x) => x.label)).toEqual([
			'Cases',
			'Resolved by agents',
			'Not eligible',
			'Handed to a person',
			'Retries',
		]);
		expect(k[0]).toMatchObject({ value: 14, previous: 10 });
		expect(k[1]!.value).toBeCloseTo(8 / 14);
		expect(k[1]!.previous).toBeCloseTo(0.5);
		expect(k[2]!.value).toBeCloseTo(2 / 14);
		expect(k[3]).toMatchObject({ value: 2, previous: 4, good: 'down' });
		expect(k[4]).toMatchObject({ value: 4, previous: null });
	});
	it('analyticsKpis shows rates as unavailable when there are no cases, and no previous rate from 0 cases', () => {
		const k = analyticsKpis({
			days: [{ day: '2026-10-15', completed: 0, not_eligible: 0, failed: 0, open: 0 }],
			by_type: [],
			latency: [],
			previous: { cases: 0, completed: 0, not_eligible: 0, failed: 0 },
		});
		expect(k[1]).toMatchObject({ value: null, previous: null, unavailable: 'no cases in this period' });
		expect(k[0]).toMatchObject({ value: 0, unavailable: '' });
	});
	it('toolBudgetMs gives every faq_ tool 20 s and the rest 3 s', () => {
		expect(toolBudgetMs('faq_ask')).toBe(20_000);
		expect(toolBudgetMs('faq')).toBe(20_000);
		expect(toolBudgetMs('order_get_order')).toBe(3_000);
		expect(toolBudgetMs('faqs')).toBe(3_000);
	});
});
