// Number formatters for the care-bus UI.
export const fmt = {
	n: (v: number | null | undefined) => (v == null ? '—' : Number(v).toLocaleString('en-GB')),
	pct: (v: number | null | undefined, d = 1) => (v == null ? '—' : (v * 100).toFixed(d) + '%'),
	ms: (v: number | null | undefined) =>
		v == null ? '—' : v >= 1000 ? (v / 1000).toFixed(v >= 10000 ? 0 : 1) + ' s' : Math.round(v) + ' ms',
	s: (v: number | null | undefined) => (v == null ? '—' : v + ' s'),
	k: (v: number) =>
		v >= 1e6 ? (v / 1e6).toFixed(1) + 'M' : v >= 1e3 ? (v / 1e3).toFixed(1) + 'k' : String(v),
};

/** `p. 2`, `p. 2–3`, or `''` when the chunk has no pages. */
export function pages(start: number | null, end: number | null): string {
	if (start == null) return '';
	return end == null || end === start ? `p. ${start}` : `p. ${start}–${end}`;
}

/**
 * A search hit's citation: `Title · p. 2–3`; the source label stands in for a missing title. With neither (strata's
 * `source` may be empty) only the pages remain, never a dangling ` · `.
 */
export function citation(hit: {
	title: string | null;
	source: string;
	page_start: number | null;
	page_end: number | null;
}): string {
	const p = pages(hit.page_start, hit.page_end);
	const name = hit.title?.trim() || hit.source.trim();
	return [name, p].filter(Boolean).join(' · ');
}
