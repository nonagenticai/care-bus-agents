import type { Me, TenantRole } from '../../src/lib/api/types';

export const BOOT_TENANT = '0195f48b-d000-7000-8000-000000000099';

/** A `GET /api/me` body for a person in the bootstrap tenant with `role`; `null` = not provisioned. */
export function meAs(role: TenantRole | null, sub = 'u1'): Me {
	return {
		sub,
		actor: sub,
		super_admin: false,
		release_writer: false,
		tenant: role ? { id: BOOT_TENANT, name: 'Bootstrap', role, status: 'active' } : null,
		active_tenant: role ? BOOT_TENANT : null,
	};
}
