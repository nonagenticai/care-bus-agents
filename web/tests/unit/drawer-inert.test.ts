import { cleanup, render } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, describe, expect, it } from 'vitest';
import Drawer from '../../src/lib/ui/Drawer.svelte';

let app: HTMLElement;
function mountApp() {
	app = document.createElement('div');
	app.id = 'app';
	document.body.append(app);
}
afterEach(() => {
	cleanup();
	app?.remove();
	document.body.style.overflow = '';
});

describe('Drawer modal isolation', () => {
	it('makes the background inert and locks scroll while open, restores on close', async () => {
		mountApp();
		document.body.style.overflow = 'auto';
		const r = render(Drawer, { open: true, title: 'A' });
		await tick();
		expect(app.hasAttribute('inert')).toBe(true);
		expect(document.body.style.overflow).toBe('hidden');
		// the drawer itself and its scrim stay interactive
		expect(r.container.hasAttribute('inert')).toBe(false);
		expect(r.container.querySelector('.drawer')!.closest('[inert]')).toBeNull();
		expect(r.container.querySelector('.scrim')!.closest('[inert]')).toBeNull();
		await r.rerender({ open: false });
		await tick();
		expect(app.hasAttribute('inert')).toBe(false);
		expect(document.body.style.overflow).toBe('auto');
	});

	it('preserves a pre-existing inert attribute', async () => {
		mountApp();
		app.setAttribute('inert', '');
		const r = render(Drawer, { open: true, title: 'A' });
		await tick();
		await r.rerender({ open: false });
		await tick();
		expect(app.hasAttribute('inert')).toBe(true);
	});

	it('handles stacked drawers: background stays inert until the last closes', async () => {
		mountApp();
		const a = render(Drawer, { open: true, title: 'A' });
		await tick();
		const target = document.createElement('div');
		document.body.append(target);
		const b = render(Drawer, { open: true, title: 'B' }, { baseElement: target });
		await tick();
		await b.rerender({ open: false });
		await tick();
		expect(app.hasAttribute('inert')).toBe(true);
		expect(document.body.style.overflow).toBe('hidden');
		await a.rerender({ open: false });
		await tick();
		expect(app.hasAttribute('inert')).toBe(false);
		expect(document.body.style.overflow).toBe('');
		target.remove();
	});
});
