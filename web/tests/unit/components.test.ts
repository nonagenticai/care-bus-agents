import { cleanup, render, screen, fireEvent } from '@testing-library/svelte';
import { tick } from 'svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import Kpi from '../../src/lib/ui/Kpi.svelte';
import Status from '../../src/lib/ui/Status.svelte';
import Drawer from '../../src/lib/ui/Drawer.svelte';
import Unavailable from '../../src/lib/ui/Unavailable.svelte';
import BarsH from '../../src/lib/ui/BarsH.svelte';
import Toast from '../../src/lib/ui/Toast.svelte';
import JsonView from '../../src/lib/ui/JsonView.svelte';
import Callout from '../../src/lib/ui/Callout.svelte';
import { createRawSnippet } from 'svelte';
import { toast, toastState } from '../../src/lib/ui/toaster.svelte';

afterEach(cleanup);

describe('ui components', () => {
	it('Kpi shows "—" and the reason when unavailable, never 0', () => {
		render(Kpi, { label: 'Cases today', value: null, unavailable: 'case_history unreachable' });
		expect(screen.getByText('—')).toBeTruthy();
		expect(screen.getByText(/unavailable: case_history unreachable/i)).toBeTruthy();
		expect(screen.queryByText('0')).toBeNull();
	});
	it('Status maps version_status words like the design', () => {
		const { container } = render(Status, { word: 'CANARY' });
		expect(container.querySelector('.st-warn')).toBeTruthy();
	});
	it('Drawer closes on Escape', async () => {
		const { container } = render(Drawer, { open: true, title: 'Case CASE-1' });
		expect(container.querySelector('.drawer')).toBeTruthy();
		await fireEvent.keyDown(window, { key: 'Escape' });
		expect(container.querySelector('.drawer')).toBeNull();
	});
	it('Unavailable renders a retry button when given one', async () => {
		let n = 0;
		render(Unavailable, { reason: 'strata_rag timed out', retry: () => n++ });
		await fireEvent.click(screen.getByRole('button', { name: 'Try again' }));
		expect(n).toBe(1);
	});
});

describe('Kpi and BarsH fix round', () => {
	it('Kpi with unavailable renders "—" and the reason, never the value 0', () => {
		render(Kpi, { label: 'Queue depth', value: '0', unavailable: 'redis down' });
		expect(screen.getByText('—')).toBeTruthy();
		expect(screen.getByText(/unavailable: redis down/i)).toBeTruthy();
		expect(screen.queryByText('0')).toBeNull();
	});
	it('BarsH rows are not focusable without onClick, and clicks pass the index when clickable', async () => {
		const items = [
			{ label: 'A', parts: [{ v: 1, cls: 'x', label: 'a' }] },
			{ label: 'B', parts: [{ v: 2, cls: 'x', label: 'b' }] },
		];
		const plain = render(BarsH, { items });
		expect(screen.queryByRole('button')).toBeNull();
		plain.unmount();
		let clicked = -1;
		render(BarsH, { items, onClick: (i: number) => (clicked = i) });
		await fireEvent.click(screen.getByText('B', { selector: '.bar-label' }).closest('button') as HTMLElement);
		expect(clicked).toBe(1);
	});
});

describe('Drawer dialog semantics', () => {
	const settle = async () => {
		await tick();
		await new Promise((r) => setTimeout(r, 0));
		await tick();
	};
	it('is a modal dialog labelled by its title, and takes focus on open', async () => {
		render(Drawer, { open: true, title: 'Case CASE-1' });
		await settle();
		const dlg = screen.getByRole('dialog', { name: 'Case CASE-1' });
		expect(dlg.getAttribute('aria-modal')).toBe('true');
		expect(dlg.contains(document.activeElement)).toBe(true);
	});
	it('traps Tab inside the dialog', async () => {
		render(Drawer, { open: true, title: 'Trap' });
		await settle();
		const close = screen.getByRole('button', { name: 'Close details' });
		close.focus();
		// Only one focusable: Tab and Shift+Tab both stay on it.
		await fireEvent.keyDown(window, { key: 'Tab' });
		expect(document.activeElement).toBe(close);
		await fireEvent.keyDown(window, { key: 'Tab', shiftKey: true });
		expect(document.activeElement).toBe(close);
	});
	it('restores focus to the opener on close', async () => {
		const opener = document.createElement('button');
		opener.textContent = 'open it';
		document.body.append(opener);
		opener.focus();
		render(Drawer, { open: true, title: 'Restore' });
		await settle();
		expect(document.activeElement).not.toBe(opener);
		await fireEvent.keyDown(window, { key: 'Escape' });
		await settle();
		expect(screen.queryByRole('dialog')).toBeNull();
		expect(document.activeElement).toBe(opener);
		opener.remove();
	});
	it('when dirty, Escape asks inside the drawer; Keep editing stays open, Discard closes', async () => {
		render(Drawer, { open: true, title: 'Edit', dirty: true });
		await settle();
		await fireEvent.keyDown(window, { key: 'Escape' });
		await settle();
		expect(screen.getByRole('dialog')).toBeTruthy();
		const keep = screen.getByRole('button', { name: 'Keep editing' });
		expect(document.activeElement).toBe(keep);
		await fireEvent.click(keep);
		expect(screen.queryByRole('button', { name: 'Keep editing' })).toBeNull();
		expect(screen.getByRole('dialog')).toBeTruthy();
		await fireEvent.click(screen.getByRole('button', { name: 'Close details' }));
		await settle();
		await fireEvent.click(screen.getByRole('button', { name: 'Discard' }));
		await settle();
		expect(screen.queryByRole('dialog')).toBeNull();
	});
	it('when dirty, the scrim asks too (never window.confirm)', async () => {
		const confirmSpy = vi.spyOn(window, 'confirm');
		const { container } = render(Drawer, { open: true, title: 'Edit', dirty: true });
		await settle();
		await fireEvent.click(container.querySelector('.scrim') as HTMLElement);
		await settle();
		expect(screen.getByRole('alertdialog', { name: 'Discard changes?' })).toBeTruthy();
		expect(screen.getByRole('dialog')).toBeTruthy();
		expect(confirmSpy).not.toHaveBeenCalled();
		confirmSpy.mockRestore();
	});
});

describe('Toast kind', () => {
	it('defaults to ok with role=status', async () => {
		render(Toast);
		toast('Saved.');
		await tick();
		expect(toastState.kind).toBe('ok');
		expect(screen.getByRole('status').textContent?.trim()).toBe('Saved.');
		expect(screen.queryByRole('alert')).toBeNull();
	});
	it('err uses role=alert and error styling', async () => {
		render(Toast);
		toast('Not saved: boom', 'err');
		await tick();
		const alert = screen.getByRole('alert');
		expect(alert.textContent?.trim()).toBe('Not saved: boom');
		expect(alert.classList.contains('err')).toBe(true);
	});
});

describe('JsonView', () => {
	it('caps rendered size and offers Show all', async () => {
		const value = { blob: 'x'.repeat(500) };
		const full = JSON.stringify(value, null, 2);
		const { container } = render(JsonView, { value, cap: 100 });
		const pre = container.querySelector('pre') as HTMLElement;
		expect(pre.textContent).toBe(full.slice(0, 100) + '…');
		await fireEvent.click(screen.getByRole('button', { name: 'Show all' }));
		expect(pre.textContent).toBe(full);
		expect(screen.queryByRole('button', { name: 'Show all' })).toBeNull();
	});
	it('renders exact JSON text with token colouring', () => {
		const value = { a: 'b', n: 1.5, t: true, z: null };
		const { container } = render(JsonView, { value });
		const pre = container.querySelector('pre') as HTMLElement;
		expect(pre.textContent).toBe(JSON.stringify(value, null, 2));
		expect(pre.querySelector('.k')?.textContent).toBe('"a"');
		expect(pre.querySelector('.s')?.textContent).toBe('"b"');
		expect(pre.querySelector('.n')?.textContent).toBe('1.5');
		expect([...pre.querySelectorAll('.b')].map((e) => e.textContent)).toEqual(['true', 'null']);
	});
	it('Copy writes the full JSON to the clipboard', async () => {
		const writeText = vi.fn().mockResolvedValue(undefined);
		Object.defineProperty(navigator, 'clipboard', { value: { writeText }, configurable: true });
		const value = { blob: 'y'.repeat(300) };
		render(JsonView, { value, cap: 50 });
		await fireEvent.click(screen.getByRole('button', { name: 'Copy' }));
		expect(writeText).toHaveBeenCalledWith(JSON.stringify(value, null, 2));
	});
});

describe('Status and Callout', () => {
	it('Status lookup is case-insensitive and replaces every underscore', () => {
		const { container } = render(Status, { word: 'not_eligible_now' });
		expect(container.textContent).toBe('not eligible now');
		cleanup();
		const r = render(Status, { word: 'published' });
		expect(r.container.querySelector('.st-ok')).toBeTruthy();
	});
	it('Callout carries an icon so kind is not colour-only', () => {
		const children = createRawSnippet(() => ({ render: () => '<span>body</span>' }));
		const { container } = render(Callout, { kind: 'err', title: 'Broken', children });
		expect(container.querySelector('.callout.err svg.ic')).toBeTruthy();
	});
});
