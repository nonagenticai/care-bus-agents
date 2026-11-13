import type { Me, TenantRole } from '$lib/api/types';

/** Tenant role from care-bus's `users` table, reported by `GET /api/me`. Token roles grant nothing in the web. */
export type Role = TenantRole;
export type { Me };
/** `actor` is what carebus-api records as the actor: preferred_username, else sub. */
export type User = { sub: string; name: string; actor?: string };

export class Session {
	user = $state<User | null>(null);
	me = $state<Me | null>(null);
	set(user: User | null, me: Me | null) {
		this.user = user;
		this.me = me;
	}
	get superAdmin(): boolean {
		return this.me?.super_admin === true;
	}
	/**
	 * The role the API applies to this caller's requests: a super_admin acts as `tenant_admin` once a tenant is
	 * active (carebus-api tenancy.rs `resolve`); a person acts with the role of their home tenant; nobody else
	 * (unprovisioned, release-writer, super_admin with no tenant) has a role.
	 */
	get role(): Role | null {
		const me = this.me;
		if (!me) return null;
		if (me.super_admin) return me.active_tenant ? 'tenant_admin' : null;
		return me.tenant?.role ?? null;
	}
	/** `tenant_admin` implies `tenant_user`. */
	can(role: Role): boolean {
		const r = this.role;
		if (r === 'tenant_admin') return true;
		return role === 'tenant_user' && r === 'tenant_user';
	}
	/** Demo-backends writes (reset, scenarios, faults, record edits) are super_admin-only (routes/demo.rs `SuperAdminIn`). */
	get canWriteDemo(): boolean {
		return this.superAdmin && this.role === 'tenant_admin';
	}
}

export const session = new Session();
