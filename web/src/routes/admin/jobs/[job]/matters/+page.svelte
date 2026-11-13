<script lang="ts">
	// Legal matters (plan 18 compliance): the ethical wall around legal research. Lists the matters the caller may
	// see (members; a tenant_admin sees every matter of the job), creates one (the creator becomes its lead), and
	// opens one in a drawer with its members and stores (routes::legal_matters).
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { session } from '$lib/auth/session.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		MATTER_NAME_MAX,
		MATTER_REFERENCE_MAX,
		MATTER_ROLE_LABEL,
		emptyMatterDraft,
		matterProblem,
		toMatterInput,
		validateMatterDraft,
		type LegalMatter,
		type MatterDraft,
	} from '$lib/api/legal';
	import { when } from '$lib/api/research';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import MatterDetail from './MatterDetail.svelte';

	const canWrite = $derived(session.can('tenant_user'));
	const isAdmin = $derived(session.can('tenant_admin'));

	// ---- list ----
	let mine = $state(false);
	let openOnly = $state(false);
	let rows = $state<LegalMatter[]>([]);
	let listState = $state<'loading' | 'ok' | 'unavailable'>('loading');
	let listError = $state('');
	let gen = 0;

	async function loadMatters() {
		const g = ++gen;
		listState = 'loading';
		try {
			const p = await api().legalMatters({ mine: mine || undefined, open: openOnly || undefined });
			if (g !== gen) return;
			rows = p.items;
			listState = 'ok';
		} catch (e) {
			if (g !== gen) return;
			listError = e instanceof Error ? e.message : String(e);
			listState = 'unavailable';
		}
	}
	$effect(() => {
		void mine;
		void openOnly;
		void loadMatters();
	});

	/** A matter changed in the drawer (or just created) updates its row; `null` = it was deleted. */
	function changed(id: string, m: LegalMatter | null) {
		if (!m) {
			rows = rows.filter((r) => r.id !== id);
			open = false;
			return;
		}
		const i = rows.findIndex((r) => r.id === id);
		if (i >= 0) rows[i] = m;
		else rows = [m, ...rows];
	}

	// ---- new matter ----
	let draft = $state<MatterDraft>(emptyMatterDraft());
	let touched = $state(false);
	let creating = $state(false);
	let failure = $state('');
	const errors = $derived(validateMatterDraft(draft));
	const msg = (f: keyof MatterDraft) => (touched ? (errors[f] ?? '') : '');

	async function create(e: Event) {
		e.preventDefault();
		touched = true;
		if (!canWrite || creating || Object.keys(errors).length) return;
		creating = true;
		failure = '';
		try {
			const m = await api().createLegalMatter(toMatterInput(draft));
			toast('Matter created; you are its lead.');
			draft = emptyMatterDraft();
			touched = false;
			changed(m.id, m);
			listState = 'ok';
			show(m.id);
		} catch (err) {
			failure =
				err instanceof ApiError
					? `${matterProblem(err.code) ?? err.message} (${err.code})`
					: err instanceof Error
						? err.message
						: String(err);
		} finally {
			creating = false;
		}
	}

	// ---- detail drawer ----
	let open = $state(false);
	let detailId = $state('');
	function show(id: string) {
		detailId = id;
		open = true;
	}
</script>

<header class="page-head">
	<div>
		<h1>Legal matters</h1>
		<p>
			A matter walls off legal research: its runs, claims and citations are visible to its members only. A
			tenant admin manages membership but does not read a matter's research unless a lead adds them.
		</p>
	</div>
</header>

<Card title="New matter" sub="You become the matter's lead and can add colleagues and knowledge stores.">
	{#if !canWrite}
		<Callout kind="info" title="Read only">Creating a matter needs a role in this tenant.</Callout>
	{/if}
	<form onsubmit={create} aria-label="New legal matter">
		<fieldset class="lm-form" disabled={!canWrite || creating}>
			<div class="form-grid">
				<label class="field"
					><span class="label">Name</span>
					<input
						class="input"
						bind:value={draft.name}
						maxlength={MATTER_NAME_MAX}
						autocomplete="off"
						aria-invalid={msg('name') ? 'true' : undefined}
					/>
					{#if msg('name')}<span class="field-err">{msg('name')}</span>{/if}
				</label>
				<label class="field"
					><span class="label">Reference</span>
					<input
						class="input"
						bind:value={draft.reference}
						maxlength={MATTER_REFERENCE_MAX}
						placeholder="e.g. I C 12/26"
						autocomplete="off"
						aria-invalid={msg('reference') ? 'true' : undefined}
					/>
					{#if msg('reference')}<span class="field-err">{msg('reference')}</span>{/if}
				</label>
			</div>
		</fieldset>
		{#if failure}<Callout kind="err" title="Not created">{failure}</Callout>{/if}
		<div class="actions">
			<button class="btn primary" type="submit" disabled={!canWrite || creating}
				>{creating ? 'Creating…' : 'Create matter'}</button
			>
		</div>
	</form>
</Card>

<Card title="Matters" cls="flush">
	{#snippet actions()}
		<label class="toggle"><input type="checkbox" bind:checked={mine} /> Mine only</label>
		<label class="toggle"><input type="checkbox" bind:checked={openOnly} /> Open only</label>
	{/snippet}
	{#if listState === 'loading'}
		<p class="muted lm-pad">Loading…</p>
	{:else if listState === 'unavailable'}
		<Unavailable reason={listError} retry={loadMatters} />
	{:else if rows.length === 0}
		<div class="empty">{mine || openOnly ? 'No matters match.' : 'No legal matters yet.'}</div>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl" aria-label="Legal matters">
				<thead>
					<tr
						><th>Matter</th><th>Reference</th><th>Your role</th><th class="r">Members</th><th class="r"
							>Stores</th
						><th>Created</th><th>State</th></tr
					>
				</thead>
				<tbody>
					{#each rows as r (r.id)}
						<tr>
							<td><button class="table-action" type="button" onclick={() => show(r.id)}>{r.name}</button></td>
							<td>{r.reference ?? '—'}</td>
							<td>{r.my_role ? MATTER_ROLE_LABEL[r.my_role] : isAdmin ? 'Admin (no access to runs)' : '—'}</td
							>
							<td class="r">{r.member_count}</td>
							<td class="r">{r.store_count}</td>
							<td>{when(r.created_at)}</td>
							<td>{r.closed_at ? 'Closed' : 'Open'}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</Card>

<Drawer bind:open title="Legal matter" wide>
	{#if open && detailId}
		<MatterDetail id={detailId} {isAdmin} onchange={changed} />
	{/if}
</Drawer>

<style>
	.lm-form {
		display: grid;
		gap: 12px;
		border: 0;
		padding: 0;
		margin: 0;
	}
	.lm-pad {
		padding: 12px 16px;
	}
</style>
