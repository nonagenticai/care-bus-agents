import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';

describe('app.css', () => {
	it('carries the design tokens in both themes', () => {
		const css = readFileSync('src/app.css', 'utf8');
		// One source of truth per token: light-dark(light, dark). The OS preference applies through
		// `color-scheme: light dark`; the toggle pins it with [data-theme].
		for (const token of [
			'--cust',
			'--work',
			'--tool',
			'--dead',
			'--accent',
			'--bg',
			'--fg',
			'--faint',
			'--warn',
		]) {
			expect(css).toMatch(new RegExp(`${token}:light-dark\\(#[0-9a-f]{6},#[0-9a-f]{6}\\)`));
			// Defined exactly once, so the themes cannot drift apart.
			expect(css.match(new RegExp(`${token}:`, 'g'))).toHaveLength(1);
		}
		expect(css).toContain('color-scheme:light dark');
		expect(css).toContain(':root[data-theme="dark"]{color-scheme:dark}');
		expect(css).toContain(':root[data-theme="light"]{color-scheme:light}');
		expect(css).toContain('[hidden]{display:none!important}');
	});
});
