import { describe, expect, it } from 'vitest';
import { filterTenants, isLastAdmin, tenantName } from '../../src/lib/tenants';
import type { TenantSummary } from '../../src/lib/api/types';

const T: TenantSummary[] = [
	{ id: '0195f48b-d000-7000-8000-000000000099', name: 'Bootstrap', status: 'active' },
	{ id: '7f1c9a2e-0000-4000-8000-00000000fe12', name: 'Acme Retail', status: 'suspended' },
];

describe('tenant picker helpers', () => {
	it('filters by name or id, case-insensitive; blank keeps all', () => {
		expect(filterTenants(T, 'acme').map((t) => t.name)).toEqual(['Acme Retail']);
		expect(filterTenants(T, '000099').map((t) => t.name)).toEqual(['Bootstrap']);
		expect(filterTenants(T, '  ')).toHaveLength(2);
		expect(filterTenants(T, 'nobody')).toEqual([]);
	});
	it('names a tenant, falling back to its id', () => {
		expect(tenantName(T, T[1]!.id)).toBe('Acme Retail');
		expect(tenantName(T, 'x')).toBe('x');
		expect(tenantName(T, null)).toBeNull();
	});
});

describe('isLastAdmin', () => {
	it('is true only for the single tenant_admin', () => {
		const one = [
			{ sub: 'a', tenant_role: 'tenant_admin' as const },
			{ sub: 'u', tenant_role: 'tenant_user' as const },
		];
		expect(isLastAdmin(one, 'a')).toBe(true);
		expect(isLastAdmin(one, 'u')).toBe(false);
		expect(isLastAdmin([...one, { sub: 'b', tenant_role: 'tenant_admin' as const }], 'a')).toBe(false);
	});
});
