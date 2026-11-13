<script lang="ts">
	// Research (plan 18): start a cited research or Polish legal research run in this job, list the runs, and follow
	// one in a drawer. Any member of the tenant may start and cancel runs (routes::research `JobMember`). A legal run
	// is filed under one of the caller's open matters and searches only that matter's stores; the server lists only
	// the runs the caller may see (a matter's runs are its members' only).
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { KnowledgeStoreItem, Paged } from '$lib/api/types';
	import {
		BUDGET_KEYS,
		BUDGET_RULES,
		LEGAL_BANNER,
		PROFILES,
		PROFILE_LABEL,
		QUESTION_MAX,
		RUN_STATUSES,
		STATUS_LABEL,
		createProblem,
		emptyResearchDraft,
		offerableStores,
		questionSnippet,
		statusWord,
		toResearchInput,
		validateResearchDraft,
		when,
		type ResearchDraft,
		type ResearchRun,
		type ResearchRunSummary,
		type RunStatus,
	} from '$lib/api/research';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import RunDetail from './RunDetail.svelte';
	import { matterLabel, type LegalMatter } from '$lib/api/legal';
	import { MATTERS_ENABLED } from '$lib/features';

	const ROLE_HINT = 'Needs a role in this tenant';
	const canWrite = $derived(session.can('tenant_user'));

	// ---- the job's knowledge stores (only those bound to a strata store can be searched) ----
	const stores = new Resource<Paged<KnowledgeStoreItem>>(() => api().knowledgeStores());
	stores.refresh();
	const storeChoices = $derived(
		stores.value.state === 'ok'
			? stores.value.data.items.filter(
					(s): s is KnowledgeStoreItem & { strata_store_id: string } => !!s.strata_store_id,
				)
			: [],
	);

	// ---- the caller's open matters (a legal run is filed under one) ----
	let matters = $state<LegalMatter[]>([]);
	let mattersError = $state('');
	if (MATTERS_ENABLED)
		api()
			.legalMatters({ mine: true, open: true })
			.then((p) => (matters = p.items))
			.catch((e) => (mattersError = e instanceof Error ? e.message : String(e)));
	const matterName = (id: string | null) => {
		if (!id) return '—';
		const m = matters.find((x) => x.id === id);
		return m ? matterLabel(m) : 'Matter';
	};
	/** The chosen matter's attached store ids; null = no matter (every store is offered). */
	let matterStores = $state<string[] | null>(null);
	let matterGen = 0;
	async function chooseMatter(id: string) {
		draft.matter_id = id;
		draft.store_ids = [];
		const g = ++matterGen;
		if (!id) {
			matterStores = null;
			return;
		}
		matterStores = [];
		try {
			const d = await api().legalMatter(id);
			if (g === matterGen) matterStores = d.stores.map((x) => x.store_id);
		} catch (e) {
			if (g === matterGen)
				failure = `The matter could not be read: ${e instanceof Error ? e.message : String(e)}`;
		}
	}
	const offered = $derived(offerableStores(storeChoices, matterStores));

	// ---- new run ----
	let draft = $state<ResearchDraft>(emptyResearchDraft());
	let touched = $state(false);
	let starting = $state(false);
	let failure = $state('');
	const errors = $derived(validateResearchDraft(draft));
	const msg = (f: keyof typeof errors) => (touched ? (errors[f] ?? '') : '');

	function toggleStore(id: string, on: boolean) {
		draft.store_ids = on ? [...draft.store_ids, id] : draft.store_ids.filter((x) => x !== id);
	}

	async function start(e: Event) {
		e.preventDefault();
		touched = true;
		if (!canWrite || starting || Object.keys(errors).length) return;
		starting = true;
		failure = '';
		try {
			const run = await api().createResearchRun(toResearchInput(draft));
			toast('Run queued.');
			const keep = draft.matter_id;
			draft = { ...emptyResearchDraft(), profile: draft.profile, matter_id: keep };
			touched = false;
			upsert(run);
			show(run.id);
		} catch (err) {
			failure =
				err instanceof ApiError
					? `${createProblem(err.code) ?? err.message} (${err.code})`
					: err instanceof Error
						? err.message
						: String(err);
		} finally {
			starting = false;
		}
	}

	// ---- runs list ----
	let status = $state<'' | RunStatus>('');
	let matterFilter = $state('');
	let rows = $state<ResearchRunSummary[]>([]);
	let next = $state<string | null>(null);
	let listState = $state<'loading' | 'ok' | 'unavailable'>('loading');
	let listError = $state('');
	let loadingMore = $state(false);
	let listGen = 0;

	async function loadRuns() {
		const g = ++listGen;
		listState = 'loading';
		try {
			const p = await api().researchRuns({
				status: status || undefined,
				matter_id: matterFilter || undefined,
			});
			if (g !== listGen) return;
			rows = p.items;
			next = p.next;
			listState = 'ok';
		} catch (e) {
			if (g !== listGen) return;
			listError = e instanceof Error ? e.message : String(e);
			listState = 'unavailable';
		}
	}
	$effect(() => {
		void status;
		void matterFilter;
		void loadRuns();
	});

	async function more() {
		if (!next || loadingMore) return;
		loadingMore = true;
		const g = listGen;
		try {
			const p = await api().researchRuns({
				status: status || undefined,
				matter_id: matterFilter || undefined,
				before: next,
			});
			if (g !== listGen) return;
			rows = [...rows, ...p.items.filter((r) => !rows.some((x) => x.id === r.id))];
			next = p.next;
		} catch (e) {
			toast(`More runs could not be read: ${e instanceof Error ? e.message : String(e)}`, 'err');
		} finally {
			loadingMore = false;
		}
	}

	/** A run read in the drawer (or just created) updates its row; a new run goes on top. */
	function upsert(run: ResearchRun) {
		const row: ResearchRunSummary = {
			id: run.id,
			profile: run.profile,
			matter_id: run.matter_id,
			question: run.question,
			status: run.status,
			steps_used: run.steps_used,
			tokens_used: run.tokens_used,
			created_at: run.created_at,
			finished_at: run.finished_at,
		};
		const i = rows.findIndex((r) => r.id === run.id);
		if (i >= 0) rows[i] = row;
		else if ((!status || status === run.status) && (!matterFilter || matterFilter === run.matter_id)) {
			rows = [row, ...rows];
			listState = 'ok';
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
		<h1>Research</h1>
		<p>
			Long-running research with cited sources. Every citation is checked against its source by code, not a
			model.
		</p>
	</div>
</header>

<Card title="New run" sub="The run is queued and worked on in the background; follow it in the list below.">
	{#if !canWrite}
		<Callout kind="info" title="Read only">Starting a run needs a role in this tenant.</Callout>
	{/if}
	<form onsubmit={start} aria-label="New research run">
		<fieldset class="rs-form" disabled={!canWrite || starting}>
			<div class="form-grid">
				<label class="field"
					><span class="label">Profile</span>
					<select class="input" bind:value={draft.profile}>
						{#each PROFILES as p (p)}<option value={p}>{PROFILE_LABEL[p]}</option>{/each}
					</select>
				</label>
			</div>
			{#if draft.profile === 'legal'}
				<p class="muted">{LEGAL_BANNER}</p>
			{/if}
			{#if MATTERS_ENABLED}
				<label class="field"
					><span class="label">Matter</span>
					<select
						class="input"
						value={draft.matter_id}
						onchange={(e) => chooseMatter(e.currentTarget.value)}
						aria-invalid={msg('matter_id') ? 'true' : undefined}
					>
						<option value="">{draft.profile === 'legal' ? 'Choose a matter…' : 'None'}</option>
						{#each matters as m (m.id)}<option value={m.id}>{matterLabel(m)}</option>{/each}
					</select>
					{#if msg('matter_id')}<span class="field-err">{msg('matter_id')}</span>{/if}
					{#if mattersError}<span class="field-err">Your matters could not be read: {mattersError}</span>{/if}
					{#if draft.profile === 'legal' && matters.length === 0 && !mattersError}
						<span class="muted">You are a member of no open matter. Create one under Legal matters.</span>
					{/if}
				</label>
			{/if}
			<label class="field"
				><span class="label">Question</span>
				<textarea
					class="input"
					rows="5"
					bind:value={draft.question}
					maxlength={QUESTION_MAX}
					aria-invalid={msg('question') ? 'true' : undefined}
					aria-describedby={msg('question') ? 'rs-err-question' : undefined}
				></textarea>
				{#if msg('question')}<span class="field-err" id="rs-err-question">{msg('question')}</span>{/if}
			</label>
			<fieldset class="rs-stores">
				<legend class="label">Knowledge stores to search</legend>
				{#if stores.value.state === 'loading'}
					<span class="muted">Loading stores…</span>
				{:else if stores.value.state === 'unavailable'}
					<Unavailable reason={stores.value.reason} retry={stores.retry} />
				{:else if offered.length === 0}
					<span class="muted"
						>{draft.matter_id
							? 'No store is attached to this matter; the run uses its other tools only.'
							: 'This job has no knowledge store bound to a search index; the run uses its other tools only.'}</span
					>
				{:else}
					{#each offered as s (s.key)}
						<label class="toggle"
							><input
								type="checkbox"
								checked={draft.store_ids.includes(s.strata_store_id)}
								onchange={(e) => toggleStore(s.strata_store_id, e.currentTarget.checked)}
							/>
							{s.label} <span class="muted mono">{s.key}</span></label
						>
					{/each}
				{/if}
				{#if msg('store_ids')}<span class="field-err">{msg('store_ids')}</span>{/if}
			</fieldset>
			<details class="rs-limits">
				<summary>Limits</summary>
				<p class="muted">Blank uses the runner's default.</p>
				<div class="form-grid">
					{#each BUDGET_KEYS as k (k)}
						{@const rule = BUDGET_RULES[k]}
						<label class="field"
							><span class="label">{rule.label}</span>
							<input
								class="input"
								inputmode="numeric"
								bind:value={draft.budget[k]}
								placeholder={`${rule.min}–${rule.max}`}
								aria-invalid={msg(k) ? 'true' : undefined}
								autocomplete="off"
							/>
							{#if msg(k)}<span class="field-err">{msg(k)}</span>{/if}
						</label>
					{/each}
				</div>
			</details>
		</fieldset>
		{#if failure}<Callout kind="err" title="Not started">{failure}</Callout>{/if}
		<div class="actions">
			<button
				class="btn primary"
				type="submit"
				disabled={!canWrite || starting}
				title={canWrite ? '' : ROLE_HINT}>{starting ? 'Starting…' : 'Start run'}</button
			>
		</div>
	</form>
</Card>

<Card title="Runs" cls="flush">
	{#snippet actions()}
		{#if MATTERS_ENABLED}
			<label class="field">
				<span class="label">Matter</span>
				<select class="input" bind:value={matterFilter} aria-label="Matter filter">
					<option value="">All you can see</option>
					{#each matters as m (m.id)}<option value={m.id}>{matterLabel(m)}</option>{/each}
				</select>
			</label>
		{/if}
		<label class="field">
			<span class="label">Status</span>
			<select class="input" bind:value={status} aria-label="Status filter">
				<option value="">All</option>
				{#each RUN_STATUSES as s (s)}<option value={s}>{STATUS_LABEL[s]}</option>{/each}
			</select>
		</label>
	{/snippet}
	{#if listState === 'loading'}
		<p class="muted rs-pad">Loading…</p>
	{:else if listState === 'unavailable'}
		<Unavailable reason={listError} retry={loadRuns} />
	{:else if rows.length === 0}
		<div class="empty">{status ? 'No runs with this status.' : 'No research runs yet.'}</div>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl" aria-label="Research runs">
				<thead>
					<tr
						><th>Status</th><th>Profile</th>{#if MATTERS_ENABLED}<th>Matter</th>{/if}<th>Question</th><th
							class="r">Steps</th
						><th class="r">Tokens</th><th>Created</th></tr
					>
				</thead>
				<tbody>
					{#each rows as r (r.id)}
						<tr>
							<td><Status word={statusWord(r.status)} label={STATUS_LABEL[r.status] ?? r.status} /></td>
							<td>{PROFILE_LABEL[r.profile] ?? r.profile}</td>
							{#if MATTERS_ENABLED}<td>{matterName(r.matter_id)}</td>{/if}
							<td
								><button class="table-action" type="button" onclick={() => show(r.id)}
									>{questionSnippet(r.question)}</button
								></td
							>
							<td class="r">{fmt.n(r.steps_used)}</td>
							<td class="r">{fmt.n(r.tokens_used)}</td>
							<td>{when(r.created_at)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		{#if next}
			<div class="actions rs-pad">
				<button class="btn" type="button" disabled={loadingMore} onclick={more}
					>{loadingMore ? 'Loading…' : 'Load more'}</button
				>
			</div>
		{/if}
	{/if}
</Card>

<Drawer bind:open title="Research run" wide>
	{#if open && detailId}
		<RunDetail id={detailId} {canWrite} onchange={upsert} />
	{/if}
</Drawer>

<style>
	.rs-form {
		display: grid;
		gap: 12px;
		border: 0;
		padding: 0;
		margin: 0;
	}
	.rs-stores {
		display: grid;
		gap: 6px;
		border: 0;
		padding: 0;
		margin: 0;
	}
	.rs-limits summary {
		cursor: pointer;
		font-weight: 600;
	}
	.rs-pad {
		padding: 12px 16px;
	}
</style>
