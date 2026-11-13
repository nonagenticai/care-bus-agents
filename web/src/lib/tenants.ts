import type { TenantSummary, TenantUser } from './api/types';

/** Tenants whose name or id contains `q` (case-insensitive); all of them for a blank query. */
export function filterTenants(list: TenantSummary[], q: string): TenantSummary[] {
	const needle = q.trim().toLowerCase();
	if (!needle) return list;
	return list.filter((t) => t.name.toLowerCase().includes(needle) || t.id.toLowerCase().includes(needle));
}

/** The tenant's name when the list knows it, else its id; null for no tenant. */
export function tenantName(list: TenantSummary[], id: string | null): string | null {
	if (!id) return null;
	return list.find((t) => t.id === id)?.name ?? id;
}

/** `sub` is the tenant's only tenant_admin: a tenant admin may not remove or demote it (409 last_tenant_admin). */
export function isLastAdmin(users: Pick<TenantUser, 'sub' | 'tenant_role'>[], sub: string): boolean {
	const admins = users.filter((u) => u.tenant_role === 'tenant_admin');
	return admins.length === 1 && admins[0]!.sub === sub;
}
