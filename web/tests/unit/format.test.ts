import { describe, expect, it } from 'vitest';
import { citation, fmt, pages } from '../../src/lib/format';

describe('fmt', () => {
	it('formats like the design', () => {
		expect(fmt.n(1234)).toBe('1,234');
		expect(fmt.pct(0.884)).toBe('88.4%');
		expect(fmt.ms(320)).toBe('320 ms');
		expect(fmt.ms(4210)).toBe('4.2 s');
		expect(fmt.k(3090)).toBe('3.1k');
		expect(fmt.n(null)).toBe('—');
	});
});

describe('citation', () => {
	const hit = { title: 'Returns policy', source: 'policies/returns.pdf', page_start: 2, page_end: 3 };
	it('names the title and the page span', () => {
		expect(citation(hit)).toBe('Returns policy · p. 2–3');
	});
	it('names a single page once', () => {
		expect(citation({ ...hit, page_start: 2, page_end: 2 })).toBe('Returns policy · p. 2');
		expect(pages(4, null)).toBe('p. 4');
	});
	it('shows no pages when the document has none', () => {
		expect(citation({ ...hit, page_start: null, page_end: null })).toBe('Returns policy');
		expect(pages(null, null)).toBe('');
	});
	it('falls back to the source label when there is no title', () => {
		expect(citation({ ...hit, title: null })).toBe('policies/returns.pdf · p. 2–3');
		expect(citation({ ...hit, title: '  ' })).toBe('policies/returns.pdf · p. 2–3');
	});
	it('shows only the pages when there is neither a title nor a source label', () => {
		expect(citation({ ...hit, title: null, source: '' })).toBe('p. 2–3');
		expect(citation({ ...hit, title: '', source: '  ', page_end: 2 })).toBe('p. 2');
		expect(citation({ ...hit, title: null, source: '', page_start: null, page_end: null })).toBe('');
	});
});
