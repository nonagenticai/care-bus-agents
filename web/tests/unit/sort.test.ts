// List-table ordering and filtering: a header click flips direction, numbers order numerically, equal keys keep
// the API's order, and a missing value sorts last whichever way the column is pointed.
import { describe, expect, it } from 'vitest';
import { matchesQuery, nextSort, sortRows, type SortState } from '../../src/lib/sort';

const rows = [
	{ id: 'b', n: 10, s: 'CASE-10' },
	{ id: 'a', n: 2, s: 'CASE-2' },
	{ id: 'c', n: null, s: 'CASE-9' },
	{ id: 'd', n: 2, s: 'CASE-1' },
];
const get = (r: (typeof rows)[number], key: string) => (key === 'n' ? r.n : key === 's' ? r.s : r.id);

describe('nextSort', () => {
	it('starts a new column ascending and flips the active one', () => {
		expect(nextSort(null, 'n')).toEqual({ key: 'n', dir: 'asc' });
		const asc: SortState = { key: 'n', dir: 'asc' };
		expect(nextSort(asc, 'n')).toEqual({ key: 'n', dir: 'desc' });
		expect(nextSort(asc, 's')).toEqual({ key: 's', dir: 'asc' });
	});
});

describe('sortRows', () => {
	it('keeps the input order when nothing is sorted', () => {
		expect(sortRows(rows, null, get).map((r) => r.id)).toEqual(['b', 'a', 'c', 'd']);
	});

	it('orders numbers numerically, not as text', () => {
		expect(sortRows(rows, { key: 'n', dir: 'asc' }, get).map((r) => r.id)).toEqual(['a', 'd', 'b', 'c']);
		expect(sortRows(rows, { key: 'n', dir: 'desc' }, get).map((r) => r.id)).toEqual(['b', 'a', 'd', 'c']);
	});

	it('keeps equal values in input order (stable)', () => {
		expect(
			sortRows(rows, { key: 'n', dir: 'asc' }, get)
				.slice(0, 2)
				.map((r) => r.id),
		).toEqual(['a', 'd']);
	});

	it('puts a missing value last in both directions', () => {
		expect(sortRows(rows, { key: 'n', dir: 'asc' }, get).at(-1)?.id).toBe('c');
		expect(sortRows(rows, { key: 'n', dir: 'desc' }, get).at(-1)?.id).toBe('c');
	});

	it('orders strings with a natural collator', () => {
		expect(sortRows(rows, { key: 's', dir: 'asc' }, get).map((r) => r.s)).toEqual([
			'CASE-1',
			'CASE-2',
			'CASE-9',
			'CASE-10',
		]);
	});
});

describe('matchesQuery', () => {
	it('matches every token across fields, case-insensitively', () => {
		expect(matchesQuery('', ['anything'])).toBe(true);
		expect(matchesQuery('  ', ['anything'])).toBe(true);
		expect(matchesQuery('case-2 alice', ['CASE-2', 'Alice'])).toBe(true);
		expect(matchesQuery('case-2 bob', ['CASE-2', 'Alice'])).toBe(false);
	});

	it('treats null and numbers as text', () => {
		expect(matchesQuery('42', [null, 42])).toBe(true);
		expect(matchesQuery('none', [null])).toBe(false);
	});
});
