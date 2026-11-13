import type { ContextSpace } from '$lib/api/types';

/**
 * Areas (departments) are configuration: the `area` of the job's effective context spaces
 * (GET /api/jobs/{job}/context-spaces). Nothing here lists them; a space added in a release adds its area.
 */

/** Words for an area key: short keys are acronyms (`faq` → FAQ), others read as a phrase (`order-support` → Order support). */
export function areaLabel(a: string): string {
	if (a.length <= 3) return a.toUpperCase();
	const words = a.replaceAll('-', ' ').replaceAll('_', ' ');
	return words.charAt(0).toUpperCase() + words.slice(1);
}

/**
 * Distinct areas of `spaces` as [key, label], sorted by label. `active` (e.g. an area filter from the URL) is kept
 * selectable even when no space names it, such as a removed area whose cases are still in the history.
 */
export function areasOf(spaces: Pick<ContextSpace, 'area'>[], active = ''): [string, string][] {
	const keys = new Set(spaces.map((s) => s.area));
	if (active) keys.add(active);
	return [...keys].map((k): [string, string] => [k, areaLabel(k)]).sort((x, y) => x[1].localeCompare(y[1]));
}
