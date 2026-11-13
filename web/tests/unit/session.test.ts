import { describe, expect, it } from 'vitest';
import { Session } from '../../src/lib/auth/session.svelte';
import { meAs } from './me';

const user = { sub: 'u1', name: 'A. Bakker' };

describe('session roles come from /api/me', () => {
	it('tenant_admin implies tenant_user; a tenant_user cannot write', () => {
		const s = new Session();
		s.set(user, meAs('tenant_user'));
		expect(s.can('tenant_user')).toBe(true);
		expect(s.can('tenant_admin')).toBe(false);
		s.set(user, meAs('tenant_admin'));
		expect(s.can('tenant_user')).toBe(true);
		expect(s.can('tenant_admin')).toBe(true);
		expect(s.superAdmin).toBe(false);
		expect(s.canWriteDemo).toBe(false);
	});
	it('an unprovisioned caller (null tenant) has no role', () => {
		const s = new Session();
		s.set(user, meAs(null));
		expect(s.can('tenant_user')).toBe(false);
		expect(s.can('tenant_admin')).toBe(false);
	});
	it('a super_admin acts as tenant_admin only once a tenant is active', () => {
		const s = new Session();
		s.set(user, { ...meAs(null), super_admin: true });
		expect(s.superAdmin).toBe(true);
		expect(s.can('tenant_user')).toBe(false);
		expect(s.canWriteDemo).toBe(false);
		s.set(user, { ...meAs(null), super_admin: true, active_tenant: '7f1c9a2e-0000-4000-8000-00000000fe12' });
		expect(s.can('tenant_admin')).toBe(true);
		expect(s.canWriteDemo).toBe(true);
	});
	it('no me, no role', () => {
		const s = new Session();
		expect(s.can('tenant_user')).toBe(false);
	});
});
