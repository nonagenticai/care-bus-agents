// List-table ordering and filtering. Lists are small (a page of at most a few hundred rows), so this is
// client-side: the pages sort and filter what the API returned, and say so when a list is paged.
export type SortDir = 'asc' | 'desc';
export interface SortState {
	key: string;
	dir: SortDir;
}

const collator = new Intl.Collator(undefined, { numeric: true, sensitivity: 'base' });

/** A header click: the same key flips direction; a new key starts ascending. */
export function nextSort(state: SortState | null, key: string): SortState {
	return { key, dir: state?.key === key && state.dir === 'asc' ? 'desc' : 'asc' };
}

const missing = (v: unknown) => v === null || v === undefined || v === '';

/**
 * Rows ordered by `value(row, key)`. Equal values keep their input order (sort is stable), and missing values
 * sort last in both directions — a blank cell is not "smallest", it is "nothing to say".
 */
export function sortRows<T>(
	rows: T[],
	state: SortState | null,
	value: (row: T, key: string) => string | number | null | undefined,
): T[] {
	if (!state) return rows;
	const dir = state.dir === 'asc' ? 1 : -1;
	return [...rows].sort((a, b) => {
		const x = value(a, state.key);
		const y = value(b, state.key);
		const xm = missing(x);
		const ym = missing(y);
		if (xm || ym) return xm === ym ? 0 : xm ? 1 : -1;
		if (typeof x === 'number' && typeof y === 'number') return dir * (x - y);
		return dir * collator.compare(String(x), String(y));
	});
}

/**
 * True when every whitespace-separated token of `query` occurs in one of the fields (case-insensitive).
 * An empty query matches everything, so callers can apply it unconditionally.
 */
export function matchesQuery(query: string, fields: (string | number | null | undefined)[]): boolean {
	const tokens = query.trim().toLowerCase().split(/\s+/).filter(Boolean);
	if (tokens.length === 0) return true;
	const hay = fields.map((f) => (f === null || f === undefined ? '' : String(f).toLowerCase()));
	return tokens.every((t) => hay.some((h) => h.includes(t)));
}
