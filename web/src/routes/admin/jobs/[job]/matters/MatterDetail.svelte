<script lang="ts">
	// One legal matter: its settings (lead or tenant_admin), members panel (lead or tenant_admin; an admin who is not
	// a lead cannot add themselves) and stores panel (lead only). Every rule is the server's (routes::legal_matters);
	// the controls only reflect it.
	import { untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { session } from '$lib/auth/session.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { KnowledgeStoreItem, TenantUser } from '$lib/api/types';
	import {
		MATTER_ROLES,
		MATTER_ROLE_LABEL,
		canEditStores,
		canGrant,
		canManage,
		isLastLead,
		matterLabel,
		matterProblem,
		type LegalMatter,
		type LegalMatterDetail,
		type MatterRole,
	} from '$lib/api/legal';
	import { when } from '$lib/api/research';
	import Callout from '$lib/ui/Callout.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	let {
		id,
		isAdmin = false,
		onchange,
	}: {
		id: string;
		/** tenant_admin (or a super_admin in the tenant). */
		isAdmin?: boolean;
		/** Called with the matter after every change; `null` once it is deleted. */
		onchange?: (id: string, m: LegalMatter | null) => void;
	} = $props();

	let detail = $state<LegalMatterDetail | null>(null);
	let loadError = $state('');
	let busy = $state(false);
	let problem = $state('');
	let users = $state<TenantUser[]>([]);
	let stores = $state<KnowledgeStoreItem[]>([]);
	let gen = 0;

	const me = $derived(session.me?.sub ?? session.user?.sub ?? '');
	const manage = $derived(detail ? canManage(detail.matter, isAdmin) : false);
	const editStores = $derived(detail ? canEditStores(detail.matter) : false);
	const tenantId = $derived(session.me?.active_tenant ?? session.me?.tenant?.id ?? null);

	async function read() {
		const g = ++gen;
		try {
			const d = await api().legalMatter(id);
			if (g !== gen) return;
			detail = d;
			loadError = '';
		} catch (e) {
			if (g !== gen) return;
			loadError = e instanceof Error ? e.message : String(e);
		}
	}
	$effect(() => {
		void id;
		untrack(() => {
			detail = null;
			void read();
		});
	});
	// the pickers: tenant users (members) and the job's indexed knowledge stores
	$effect(() => {
		const t = tenantId;
		untrack(() => {
			if (t)
				api()
					.tenantUsers(t)
					.then((p) => (users = p.items))
					.catch(() => (users = []));
			api()
				.knowledgeStores()
				.then((p) => (stores = p.items.filter((s) => !!s.strata_store_id)))
				.catch(() => (stores = []));
		});
	});

	function why(err: unknown): string {
		if (err instanceof ApiError) return `${matterProblem(err.code) ?? err.message} (${err.code})`;
		return err instanceof Error ? err.message : String(err);
	}

	/** Runs one write, then reads the matter again and reports it to the list. */
	async function act(fn: () => Promise<unknown>, done: string) {
		if (busy) return;
		busy = true;
		problem = '';
		try {
			await fn();
			toast(done);
			await read();
			if (detail) onchange?.(id, detail.matter);
		} catch (err) {
			problem = why(err);
		} finally {
			busy = false;
		}
	}

	// ---- settings ----
	let editName = $state('');
	let editReference = $state('');
	$effect(() => {
		const m = detail?.matter;
		if (m)
			untrack(() => {
				editName = m.name;
				editReference = m.reference ?? '';
			});
	});
	function save(e: Event) {
		e.preventDefault();
		void act(
			() => api().updateLegalMatter(id, { name: editName.trim(), reference: editReference.trim() }),
			'Matter saved.',
		);
	}
	function setClosed(closed: boolean) {
		void act(() => api().updateLegalMatter(id, { closed }), closed ? 'Matter closed.' : 'Matter reopened.');
	}
	async function remove() {
		if (busy) return;
		busy = true;
		problem = '';
		try {
			await api().deleteLegalMatter(id);
			toast('Matter deleted.');
			onchange?.(id, null);
		} catch (err) {
			problem = why(err);
		} finally {
			busy = false;
		}
	}

	// ---- members ----
	let newSub = $state('');
	let newRole = $state<MatterRole>('member');
	const candidates = $derived(
		users.filter(
			(u) =>
				!detail?.members.some((m) => m.user_sub === u.sub) && (!detail || canGrant(detail.matter, me, u.sub)),
		),
	);
	function addMember(e: Event) {
		e.preventDefault();
		const sub = newSub.trim();
		if (!sub) return;
		void act(async () => {
			await api().setMatterMember(id, sub, newRole);
			newSub = '';
		}, 'Member added.');
	}

	// ---- stores ----
	let newStore = $state('');
	const storeLabel = (sid: string) => {
		const s = stores.find((x) => x.strata_store_id === sid);
		return s ? `${s.label} (${s.key})` : sid;
	};
	const attachable = $derived(
		stores.filter((s) => !detail?.stores.some((x) => x.store_id === s.strata_store_id)),
	);
	function attach(e: Event) {
		e.preventDefault();
		const sid = newStore;
		if (!sid) return;
		void act(async () => {
			await api().attachMatterStore(id, sid);
			newStore = '';
		}, 'Store attached.');
	}
</script>

{#if loadError && !detail}
	<Unavailable reason={loadError} retry={read} />
{:else if !detail}
	<p class="muted">Loading…</p>
{:else}
	{@const m = detail.matter}
	<section class="lm-detail" aria-label="Matter {matterLabel(m)}">
		<h2>{matterLabel(m)}</h2>
		<KeyValues
			pairs={[
				['Your role', m.my_role ? MATTER_ROLE_LABEL[m.my_role] : 'Not a member (no access to its research)'],
				['State', m.closed_at ? `Closed ${when(m.closed_at)}` : 'Open'],
				['Created', `${when(m.created_at)} by ${m.created_by}`],
			]}
		/>
		{#if problem}<Callout kind="err" title="Not done">{problem}</Callout>{/if}

		{#if manage}
			<form class="lm-row" onsubmit={save} aria-label="Matter settings">
				<label class="field"
					><span class="label">Name</span><input
						class="input"
						bind:value={editName}
						autocomplete="off"
					/></label
				>
				<label class="field"
					><span class="label">Reference</span><input
						class="input"
						bind:value={editReference}
						autocomplete="off"
					/></label
				>
				<div class="actions">
					<button class="btn" type="submit" disabled={busy || !editName.trim()}>Save</button>
					<button class="btn" type="button" disabled={busy} onclick={() => setClosed(!m.closed_at)}
						>{m.closed_at ? 'Reopen matter' : 'Close matter'}</button
					>
					<button class="btn danger" type="button" disabled={busy} onclick={remove}>Delete matter</button>
				</div>
			</form>
		{/if}

		<h3>Members</h3>
		<table class="tbl" aria-label="Members">
			<thead><tr><th>Person</th><th>Role</th><th>Added</th><th></th></tr></thead>
			<tbody>
				{#each detail.members as x (x.user_sub)}
					{@const last = isLastLead(detail.members, x.user_sub)}
					<tr>
						<td>{x.display_name ?? x.user_sub} <span class="muted mono">{x.user_sub}</span></td>
						<td>
							{#if manage && canGrant(m, me, x.user_sub)}
								<select
									class="input"
									aria-label="Role of {x.user_sub}"
									value={x.role}
									disabled={busy || last}
									title={last ? 'The only lead cannot be demoted' : ''}
									onchange={(e) => {
										const role = e.currentTarget.value as MatterRole;
										void act(() => api().setMatterMember(id, x.user_sub, role), 'Role changed.');
									}}
								>
									{#each MATTER_ROLES as r (r)}<option value={r}>{MATTER_ROLE_LABEL[r]}</option>{/each}
								</select>
							{:else}
								{MATTER_ROLE_LABEL[x.role]}
							{/if}
						</td>
						<td>{when(x.added_at)}</td>
						<td>
							{#if manage}
								<button
									class="btn"
									type="button"
									disabled={busy || last}
									title={last ? 'The only lead cannot be removed' : ''}
									aria-label="Remove {x.user_sub}"
									onclick={() => act(() => api().removeMatterMember(id, x.user_sub), 'Member removed.')}
									>Remove</button
								>
							{/if}
						</td>
					</tr>
				{/each}
			</tbody>
		</table>
		{#if manage}
			<form class="lm-row" onsubmit={addMember} aria-label="Add member">
				<label class="field"
					><span class="label">Person</span>
					{#if candidates.length}
						<select class="input" bind:value={newSub}>
							<option value="">Choose…</option>
							{#each candidates as u (u.sub)}<option value={u.sub}>{u.display_name ?? u.sub}</option>{/each}
						</select>
					{:else}
						<input class="input" bind:value={newSub} placeholder="user id (sub)" autocomplete="off" />
					{/if}
				</label>
				<label class="field"
					><span class="label">Role</span>
					<select class="input" bind:value={newRole}>
						{#each MATTER_ROLES as r (r)}<option value={r}>{MATTER_ROLE_LABEL[r]}</option>{/each}
					</select>
				</label>
				<div class="actions">
					<button class="btn primary" type="submit" disabled={busy || !newSub.trim()}>Add member</button>
				</div>
			</form>
			{#if isAdmin && m.my_role !== 'lead'}
				<p class="muted">
					As an admin you can manage this matter's members, but not add yourself: a lead must.
				</p>
			{/if}
		{/if}

		<h3>Knowledge stores</h3>
		{#if detail.stores.length === 0}
			<p class="muted">No store attached: the matter's runs search public legal sources only.</p>
		{:else}
			<ul class="lm-stores" aria-label="Matter stores">
				{#each detail.stores as st (st.store_id)}
					<li>
						{storeLabel(st.store_id)}
						{#if m.my_role === 'lead'}
							<button
								class="btn"
								type="button"
								disabled={busy}
								aria-label="Detach {storeLabel(st.store_id)}"
								onclick={() => act(() => api().detachMatterStore(id, st.store_id), 'Store detached.')}
								>Detach</button
							>
						{/if}
					</li>
				{/each}
			</ul>
		{/if}
		{#if editStores}
			<form class="lm-row" onsubmit={attach} aria-label="Attach store">
				<label class="field"
					><span class="label">Store</span>
					<select class="input" bind:value={newStore}>
						<option value="">Choose…</option>
						{#each attachable as s (s.key)}<option value={s.strata_store_id}>{s.label} ({s.key})</option
							>{/each}
					</select>
				</label>
				<div class="actions">
					<button class="btn primary" type="submit" disabled={busy || !newStore}>Attach store</button>
				</div>
			</form>
			<p class="muted">
				An attached store is walled: only runs filed under one of its matters may search it.
			</p>
		{:else if m.my_role !== 'lead'}
			<p class="muted">Only the matter's lead attaches stores.</p>
		{/if}
	</section>
{/if}

<style>
	.lm-detail {
		display: grid;
		gap: 12px;
	}
	.lm-row {
		display: flex;
		flex-wrap: wrap;
		gap: 12px;
		align-items: flex-end;
	}
	.lm-stores {
		display: grid;
		gap: 6px;
		margin: 0;
		padding-left: 18px;
	}
</style>
