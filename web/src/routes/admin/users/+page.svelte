<script lang="ts">
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { isLastAdmin } from '$lib/tenants';
	import type { Paged, TenantRole, TenantUser } from '$lib/api/types';
	import Callout from '$lib/ui/Callout.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	/** The tenant the page acts in: a super_admin's chosen tenant, else the caller's own. */
	const tenant = app.activeTenant ?? session.me?.active_tenant ?? session.me?.tenant?.id ?? '';
	const users = new Resource<Paged<TenantUser>>(() => api().tenantUsers(tenant));
	users.refresh();
	const canWrite = $derived(session.can('tenant_admin'));
	/** A tenant admin may not remove or demote the last tenant_admin; a super_admin may (routes::tenancy). */
	const guardLast = $derived(!session.superAdmin);
	const list = $derived(users.value.state === 'ok' ? users.value.data.items : []);
	const LAST = "The tenant's last tenant_admin. Add another tenant_admin first.";
	let busy = $state(false);
	let sub = $state('');
	let displayName = $state('');
	let role = $state<TenantRole>('tenant_user');

	async function run(action: () => Promise<unknown>, msg: string, fail: string): Promise<boolean> {
		if (busy) return false;
		busy = true;
		const ok = await guard(action, msg, fail);
		busy = false;
		await users.refresh();
		return ok;
	}
	async function add(e: SubmitEvent) {
		e.preventDefault();
		const s = sub.trim();
		if (!s) return;
		const ok = await run(
			() => api().saveTenantUser(tenant, s, { tenant_role: role, display_name: displayName.trim() || null }),
			`Added ${s}.`,
			'Not added',
		);
		if (ok) {
			sub = '';
			displayName = '';
			role = 'tenant_user';
		}
	}
	const setRole = (u: TenantUser, r: TenantRole) =>
		run(
			() => api().saveTenantUser(tenant, u.sub, { tenant_role: r, display_name: u.display_name }),
			`${u.display_name ?? u.sub} is now ${r}.`,
			'Role not changed',
		);
	const remove = (u: TenantUser) =>
		run(() => api().removeTenantUser(tenant, u.sub), `Removed ${u.display_name ?? u.sub}.`, 'Not removed');
	const lastAdmin = (u: TenantUser) => guardLast && isLastAdmin(list, u.sub);
	const when = (iso: string) => new Date(iso).toLocaleString('en-GB');
</script>

<header class="page-head">
	<div>
		<h1>Users</h1>
		<p>
			Who may use care-bus in this tenant. Users sign in with Keycloak; add them here by their Keycloak
			subject (sub). A tenant_admin changes configuration, a tenant_user uses the console and reads the admin
			pages.
		</p>
	</div>
</header>
{#if !canWrite}
	<Callout kind="info" title="Read only"
		>You can view the users. Managing them needs the tenant_admin role.</Callout
	>
{/if}

<Freshness resource={users} />
{#if users.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if users.value.state === 'unavailable'}
	<Unavailable reason={users.value.reason} retry={users.retry} />
{:else if users.value.state === 'empty'}
	<div class="empty">This tenant has no users yet.</div>
{:else}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl" data-testid="tenant-users">
				<thead
					><tr><th>User</th><th>Name</th><th>Role</th><th>Updated</th><th aria-label="Actions"></th></tr
					></thead
				>
				<tbody>
					{#each list as u (u.sub)}
						<tr data-sub={u.sub}>
							<td class="mono">{u.sub}</td>
							<td>{u.display_name ?? '—'}</td>
							<td>
								<select
									class="input"
									aria-label={`Role of ${u.sub}`}
									value={u.tenant_role}
									disabled={!canWrite || busy || lastAdmin(u)}
									title={!canWrite ? 'Needs the tenant_admin role' : lastAdmin(u) ? LAST : ''}
									onchange={(e) => setRole(u, e.currentTarget.value as TenantRole)}
								>
									<option value="tenant_admin">tenant_admin</option>
									<option value="tenant_user">tenant_user</option>
								</select>
							</td>
							<td>{when(u.updated_at)} · {u.updated_by}</td>
							<td class="r">
								<button
									class="btn sm"
									type="button"
									disabled={!canWrite || busy || lastAdmin(u)}
									title={!canWrite ? 'Needs the tenant_admin role' : lastAdmin(u) ? LAST : ''}
									onclick={() => remove(u)}>Remove</button
								>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

<Card title="Add a user">
	<form class="add" onsubmit={add}>
		<fieldset class="fields" disabled={!canWrite || busy}>
			<label class="field"
				><span class="label">Keycloak subject (sub)</span><input
					class="input mono"
					autocomplete="off"
					spellcheck="false"
					bind:value={sub}
				/></label
			>
			<label class="field"
				><span class="label">Display name</span><input class="input" bind:value={displayName} /></label
			>
			<label class="field"
				><span class="label">Role</span>
				<select class="input" bind:value={role}>
					<option value="tenant_user">tenant_user</option>
					<option value="tenant_admin">tenant_admin</option>
				</select>
			</label>
		</fieldset>
		<button
			class="btn primary"
			type="submit"
			disabled={!canWrite || busy || !sub.trim()}
			title={canWrite ? '' : 'Needs the tenant_admin role'}>Add user</button
		>
	</form>
</Card>

<style>
	.add {
		display: grid;
		gap: 12px;
	}
	.fields {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
		grid-template-columns: repeat(auto-fit, minmax(200px, 1fr));
	}
</style>
