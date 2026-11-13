import { cleanup, fireEvent, render, screen } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { Api } from '../../src/lib/api/client';
import { app } from '../../src/lib/app.svelte';
import type { Me } from '../../src/lib/api/types';
import { session } from '../../src/lib/auth/session.svelte';
import { meAs } from './me';
import DemoPage from '../../src/routes/demo/+page.svelte';

afterEach(() => {
	cleanup();
	app.api = null;
});

/** Demo writes are super_admin-only (routes/demo.rs `SuperAdminIn`); a tenant_admin only reads. */
const superAdmin: Me = { ...meAs('tenant_admin'), super_admin: true };

function renderAs(me: Me) {
	session.set({ sub: 'u1', name: 'A. Bakker' }, me);
	app.api = {
		demoScenarios: () => new Promise(() => {}),
		faults: () => new Promise(() => {}),
	} as unknown as Api;
	render(DemoPage);
}

const writeInputs = () => [
	screen.getByLabelText('Tool'),
	screen.getByLabelText('Mode'),
	screen.getByLabelText('Count'),
	screen.getByLabelText('Seed'),
];

describe('/demo write controls need a super_admin', () => {
	it('a tenant_admin cannot edit the fault picker or the reset seed', () => {
		renderAs(meAs('tenant_admin'));
		expect(screen.getByRole('button', { name: 'Apply' })).toHaveProperty('disabled', true);
		for (const el of writeInputs()) {
			expect(el, el.outerHTML).toHaveProperty('disabled', true);
			expect(el.getAttribute('title')).toBe('Needs the super_admin role');
		}
	});
	it('a super_admin acting in a tenant can edit them', () => {
		renderAs(superAdmin);
		expect(screen.getByRole('button', { name: 'Apply' })).toHaveProperty('disabled', false);
		for (const el of writeInputs()) expect(el, el.outerHTML).toHaveProperty('disabled', false);
	});
});

describe('/demo scenarios ask before changing records', () => {
	it('Run asks inline first; only "Yes, run" calls the API, Cancel does not', async () => {
		session.set({ sub: 'u1', name: 'A. Bakker' }, superAdmin);
		const runScenario = vi.fn(async () => ({ ok: true }));
		app.api = {
			demoScenarios: async () => ({ scenarios: [{ name: 'late_shipment', description: 'Delays a parcel' }] }),
			faults: async () => ({ faults: {} }),
			runScenario,
		} as unknown as Api;
		render(DemoPage);
		await fireEvent.click(await screen.findByRole('button', { name: 'Run…' }));
		expect(runScenario).not.toHaveBeenCalled();
		expect(screen.getByText('Changes demo records. Run it?')).toBeTruthy();
		await fireEvent.click(screen.getByRole('button', { name: 'Cancel' }));
		expect(runScenario).not.toHaveBeenCalled();
		await fireEvent.click(screen.getByRole('button', { name: 'Run…' }));
		await fireEvent.click(screen.getByRole('button', { name: 'Yes, run' }));
		expect(runScenario).toHaveBeenCalledWith('late_shipment');
	});
	it('fault tools show plain-language names but send the tool id', () => {
		renderAs(superAdmin);
		const opt = screen.getByRole('option', { name: 'Carrier: track shipment' }) as HTMLOptionElement;
		expect(opt.value).toBe('delivery_track_shipment');
	});
});
