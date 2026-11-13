import { afterEach, describe, expect, it } from 'vitest';
import { ApiError, type Api } from '../../src/lib/api/client';
import type { JobSummary, Me } from '../../src/lib/api/types';
import { app, bootstrapScope, isTenantId, pickJob, switchTenant } from '../../src/lib/app.svelte';
import { session } from '../../src/lib/auth/session.svelte';
import { BOOT_TENANT, meAs } from './me';

const OTHER = '7f1c9a2e-0000-4000-8000-00000000fe12';
const user = { sub: 'u1', name: 'A. Bakker' };
const job = (key: string): JobSummary => ({ key, label: key, settings: {}, on_bus: true });

/** A fake Api whose `me()` answers like routes::tenancy::me for the header the app would send. */
function fakeApi(me: (activeTenant: string | null) => Me, jobs: () => Promise<JobSummary[]>) {
	const calls: (string | null)[] = [];
	const a = {
		me: async () => {
			calls.push(app.activeTenant);
			return me(app.activeTenant);
		},
		jobs: async () => ({ items: await jobs(), next: null }),
	} as unknown as Api;
	app.api = a;
	return { a, calls };
}

const superAdmin = (known: string[]) => (t: string | null) => ({
	...meAs(null),
	super_admin: true,
	active_tenant: t && known.includes(t) ? t : null,
});

afterEach(() => {
	sessionStorage.clear();
	app.api = null;
	app.activeTenant = null;
	app.blocked = null;
	app.jobs = [];
	session.set(null, null);
});

describe('pickJob', () => {
	it('keeps a stored job the tenant has, else the first job, else the bootstrap job', () => {
		expect(pickJob([job('a'), job('b')], 'b')).toBe('b');
		expect(pickJob([job('a'), job('b')], 'gone')).toBe('a');
		expect(pickJob([], null)).toBe('customer_care_agent');
	});
});

describe('isTenantId', () => {
	it('accepts a bare uuid, rejects the nil uuid and other text', () => {
		expect(isTenantId(BOOT_TENANT)).toBe(true);
		expect(isTenantId('00000000-0000-0000-0000-000000000000')).toBe(false);
		expect(isTenantId('acme')).toBe(false);
	});
});

describe('bootstrapScope', () => {
	it('a provisioned person gets the first job and sends no active-tenant header', async () => {
		const { a, calls } = fakeApi(
			() => meAs('tenant_user'),
			async () => [job('customer_care_agent'), job('x')],
		);
		await bootstrapScope(a, user);
		expect(calls).toEqual([null]);
		expect(app.blocked).toBeNull();
		expect(app.job).toBe('customer_care_agent');
		expect(session.can('tenant_user')).toBe(true);
	});
	it('an unprovisioned person is blocked as not_provisioned and no jobs are asked for', async () => {
		let asked = false;
		const { a } = fakeApi(
			() => meAs(null),
			async () => {
				asked = true;
				return [];
			},
		);
		await bootstrapScope(a, user);
		expect(app.blocked).toBe('not_provisioned');
		expect(asked).toBe(false);
	});
	it('a suspended tenant blocks; a jobs() 403 tenant_suspended does too', async () => {
		const { a } = fakeApi(
			() => meAs('tenant_admin'),
			async () => {
				throw new ApiError(403, 'tenant_suspended', 'suspended');
			},
		);
		await bootstrapScope(a, user);
		expect(app.blocked).toBe('tenant_suspended');
	});
	it('a super_admin with no tenant must pick one', async () => {
		const { a } = fakeApi(superAdmin([OTHER]), async () => []);
		await bootstrapScope(a, user);
		expect(app.blocked).toBe('pick_tenant');
	});
	it('a super_admin resumes the tenant kept in sessionStorage when it still exists', async () => {
		sessionStorage.setItem('carebus-active-tenant', OTHER);
		const { a, calls } = fakeApi(superAdmin([OTHER]), async () => [job('j1')]);
		await bootstrapScope(a, user);
		expect(calls).toEqual([null, OTHER]);
		expect(app.activeTenant).toBe(OTHER);
		expect(app.blocked).toBeNull();
		expect(session.can('tenant_admin')).toBe(true);
	});
	it('a stored tenant that no longer exists is dropped', async () => {
		sessionStorage.setItem('carebus-active-tenant', OTHER);
		const { a } = fakeApi(superAdmin([]), async () => []);
		await bootstrapScope(a, user);
		expect(app.activeTenant).toBeNull();
		expect(sessionStorage.getItem('carebus-active-tenant')).toBeNull();
		expect(app.blocked).toBe('pick_tenant');
	});
});

describe('switchTenant', () => {
	it('switches to a known tenant and keeps it; refuses an unknown or malformed id', async () => {
		const { a } = fakeApi(superAdmin([OTHER]), async () => [job('j1')]);
		await bootstrapScope(a, user);
		expect(await switchTenant('acme')).toMatch(/uuid/);
		expect(await switchTenant(BOOT_TENANT)).toBe('No tenant with that id.');
		expect(app.activeTenant).toBeNull();
		expect(await switchTenant(OTHER.toUpperCase())).toBeNull();
		expect(app.activeTenant).toBe(OTHER);
		expect(sessionStorage.getItem('carebus-active-tenant')).toBe(OTHER);
		expect(app.blocked).toBeNull();
		expect(app.job).toBe('j1');
	});
});
