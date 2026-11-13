import type { Analytics, DayStats, Overview } from '$lib/api/types';

/** UI-only tile model; the API returns raw DayStats (plan 05 routes::overview). */
export type KpiView = {
	label: string;
	value: number | null;
	previous: number | null;
	unit: 'n' | 'pct' | 'ms';
	unavailable: string;
	/** Which direction is good; a rise in a failure count or a latency is bad (F24). */
	good?: 'up' | 'down';
};

const rate = (d: DayStats, n: number) => (d.cases === 0 ? null : n / d.cases);

export function overviewKpis(o: Overview): KpiView[] {
	const t = o.today,
		w = o.week_ago;
	const noCases = t.cases === 0 ? 'no cases yet' : '';
	return [
		{ label: 'Cases today', value: t.cases, previous: w.cases, unit: 'n', unavailable: '' },
		{
			label: 'Resolved by agents',
			value: rate(t, t.completed),
			previous: rate(w, w.completed),
			unit: 'pct',
			unavailable: noCases,
		},
		{
			label: 'Not eligible',
			value: rate(t, t.not_eligible),
			previous: rate(w, w.not_eligible),
			unit: 'pct',
			unavailable: noCases,
		},
		{
			label: 'Handed to a person',
			value: t.failed,
			previous: w.failed,
			unit: 'n',
			unavailable: '',
			good: 'down',
		},
		{
			label: 'Median time to answer',
			value: t.median_ms,
			previous: w.median_ms,
			unit: 'ms',
			unavailable: t.median_ms == null ? 'no resolved cases yet' : '',
			good: 'down',
		},
	];
}

export function kpiDelta(k: KpiView): number | null {
	if (k.value == null || k.previous == null || k.previous === 0) return null;
	return (k.value - k.previous) / k.previous;
}

export function toSeries(a: Analytics) {
	return [
		{ key: 'completed', label: 'Resolved by agents', cls: 's-ok', values: a.days.map((d) => d.completed) },
		{ key: 'not_eligible', label: 'Not eligible', cls: 's-warn', values: a.days.map((d) => d.not_eligible) },
		{ key: 'failed', label: 'Handed to a person', cls: 's-err', values: a.days.map((d) => d.failed) },
	];
}

/**
 * Tiles for the analytics range: totals over `days` against `previous` (the same number of days before it,
 * routes/analytics.rs). Open cases count towards Cases, as they do in `previous.cases`.
 */
export function analyticsKpis(a: Analytics): KpiView[] {
	const sum = (k: 'completed' | 'not_eligible' | 'failed' | 'open') => a.days.reduce((n, d) => n + d[k], 0);
	const completed = sum('completed'),
		notEligible = sum('not_eligible'),
		failed = sum('failed');
	const cases = completed + notEligible + failed + sum('open');
	const p = a.previous;
	const share = (n: number, of: number) => (of === 0 ? null : n / of);
	const noCases = cases === 0 ? 'no cases in this period' : '';
	return [
		{ label: 'Cases', value: cases, previous: p.cases, unit: 'n', unavailable: '' },
		{
			label: 'Resolved by agents',
			value: share(completed, cases),
			previous: share(p.completed, p.cases),
			unit: 'pct',
			unavailable: noCases,
		},
		{
			label: 'Not eligible',
			value: share(notEligible, cases),
			previous: share(p.not_eligible, p.cases),
			unit: 'pct',
			unavailable: noCases,
		},
		{
			label: 'Handed to a person',
			value: failed,
			previous: p.failed,
			unit: 'n',
			unavailable: '',
			good: 'down',
		},
		{
			label: 'Retries',
			value: a.by_type.reduce((n, t) => n + t.retries, 0),
			previous: null,
			unit: 'n',
			unavailable: '',
			good: 'down',
		},
	];
}

/**
 * Default tool-call timeout (crates/carebus/src/executor.rs tool_timeout): 20 s for the FAQ tool, whose answers
 * come from a model, 3 s for every other tool. Latency rows name MCP tools (`faq_ask`, `order_get_order`), so
 * the FAQ budget applies to every `faq_` tool. Deployments can override both with TOOL_TIMEOUT_MS(_FAQ).
 */
export const toolBudgetMs = (tool: string) => (tool === 'faq' || tool.startsWith('faq_') ? 20_000 : 3_000);
