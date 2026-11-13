<script lang="ts">
	import { jobPath } from '$lib/scope';
	import { api, app, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Latest, Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { citation, fmt } from '$lib/format';
	import { storeTree } from '$lib/sources';
	import type { Paged, QueryResult, StoreDocument, StoreInput, StoreNode } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const stores = new Resource<Paged<StoreNode>>(() => api().stores());
	stores.refresh();
	const canWrite = $derived(session.can('tenant_admin'));
	const why = $derived(canWrite ? '' : 'Needs the tenant_admin role');
	let sel = $state<StoreNode | null>(null);
	let recursive = $state(false);
	let docs = $state<{
		s: 'idle' | 'loading' | 'ok' | 'err';
		items: StoreDocument[];
		next: string | null;
		err: string;
	}>({ s: 'idle', items: [], next: null, err: '' });
	let loadingMore = $state(false);
	let confirmStore = $state(false);
	let confirmDoc = $state<StoreDocument | null>(null);
	let form = $state<StoreInput>({ parent_id: null, kind: 'leaf', name: '', description: '' });
	let q = $state('How long does a refund take?');
	let mode = $state('hybrid');
	let topK = $state(8);
	// strata_rag query defaults: how many router branches to follow, and the lowest relevance score to keep.
	let beam = $state(3);
	let minScore = $state(0.35);
	let usedMinScore = $state(0.35);
	let result = $state<QueryResult | null>(null);
	let queryErr = $state('');
	let running = $state(false);
	const routers = $derived(
		stores.value.state === 'ok' ? stores.value.data.items.filter((s) => s.kind === 'router') : [],
	);
	const score = (v: number | null) => (v == null ? '—' : v.toFixed(2));
	const byId = $derived(
		new Map(stores.value.state === 'ok' ? stores.value.data.items.map((s) => [s.id, s] as const) : []),
	);
	const storeName = (id: string) => byId.get(id)?.name ?? id.slice(0, 8);
	const storePath = (n: StoreNode) => {
		const out = [n.name];
		let p = n.parent_id ? byId.get(n.parent_id) : undefined;
		while (p && out.length < 32) {
			out.unshift(p.name);
			p = p.parent_id ? byId.get(p.parent_id) : undefined;
		}
		return out.join(' / ');
	};
	const docName = (d: StoreDocument) => d.title ?? d.source ?? 'Untitled document';
	const clamp = (v: unknown, lo: number, hi: number, dflt: number) => {
		const n = Number(v);
		return Number.isFinite(n) ? Math.min(hi, Math.max(lo, n)) : dflt;
	};

	const latestDocs = new Latest();
	async function pick(n: StoreNode) {
		const ticket = latestDocs.next();
		sel = n;
		recursive = false;
		confirmStore = false;
		confirmDoc = null;
		docs = { s: 'loading', items: [], next: null, err: '' };
		try {
			const page = await api().storeDocuments(n.id);
			if (!latestDocs.stale(ticket)) docs = { s: 'ok', items: page.items, next: page.next, err: '' };
		} catch (e) {
			if (!latestDocs.stale(ticket)) docs = { s: 'err', items: [], next: null, err: (e as Error).message };
		}
	}
	async function more() {
		if (!sel || !docs.next || loadingMore) return;
		const ticket = latestDocs.next();
		const before = docs.next;
		const prev = docs.items;
		loadingMore = true;
		try {
			const page = await api().storeDocuments(sel.id, before);
			if (!latestDocs.stale(ticket))
				docs = { s: 'ok', items: [...prev, ...page.items], next: page.next, err: '' };
		} catch (e) {
			if (!latestDocs.stale(ticket)) toast(`Could not load more documents: ${(e as Error).message}`);
		}
		loadingMore = false;
	}
	async function create() {
		const ok = await guard(
			() => api().createStore({ ...form, name: form.name.trim() }),
			`Store ${form.name} created.`,
		);
		if (ok) {
			form = { parent_id: form.parent_id, kind: 'leaf', name: '', description: '' };
			stores.refresh();
		}
	}
	async function removeStore() {
		if (!sel) return;
		confirmStore = false;
		const ok = await guard(
			() => api().deleteStore(sel!.id, recursive),
			`Store ${sel.name} deleted.`,
			'Not deleted',
		);
		if (ok) {
			sel = null;
			docs = { s: 'idle', items: [], next: null, err: '' };
			stores.refresh();
		}
	}
	async function removeDoc(d: StoreDocument) {
		confirmDoc = null;
		const ok = await guard(
			() => api().deleteDocument(d.id),
			'Document deleted from strata_rag.',
			'Not deleted',
		);
		if (ok && sel) pick(sel);
	}
	async function run() {
		running = true;
		queryErr = '';
		result = null;
		usedMinScore = clamp(minScore, 0, 1, 0.35);
		try {
			result = await api().query({
				query: q,
				route_mode: mode as 'embed' | 'laya' | 'hybrid',
				top_k: Math.round(clamp(topK, 1, 50, 8)),
				beam: Math.round(clamp(beam, 1, 8, 3)),
				min_score: usedMinScore,
			});
		} catch (e) {
			queryErr = (e as Error).message;
		}
		running = false;
	}
</script>

<header class="page-head">
	<div>
		<h1>Knowledge stores</h1>
		<p>The strata_rag store tree, its documents and a live search preview.</p>
	</div>
	<div class="actions">
		<Freshness resource={stores} />
		<a class="btn" href={jobPath(app.job, '/knowledge')}>Sources</a>
	</div>
</header>

<div class="grid g2">
	<Card title="Stores">
		{#if stores.value.state === 'unavailable'}
			<Unavailable reason={stores.value.reason} retry={() => stores.refresh()} />
		{:else if stores.value.state === 'ok'}
			{#each storeTree(stores.value.data.items) as { node, depth }}
				<button
					type="button"
					class="kn-node"
					class:on={sel?.id === node.id}
					style="--d:{depth}"
					onclick={() => pick(node)}
				>
					<span class="chip">{node.kind}</span>
					<span>{node.name}</span>
					<span class="muted mono">{fmt.k(node.chunk_count)} chunks · {fmt.k(node.token_count)} tokens</span>
				</button>
			{/each}
		{:else if stores.value.state === 'empty'}
			<div class="empty">No stores in strata_rag yet.</div>
		{:else}
			<p class="muted">Loading…</p>
		{/if}
		<form
			class="form-grid new-store"
			onsubmit={(e) => {
				e.preventDefault();
				create();
			}}
		>
			<label class="field"
				><span class="label">New store name</span><input class="input" bind:value={form.name} /></label
			>
			<label class="field"
				><span class="label">Kind</span>
				<select class="input" bind:value={form.kind}>
					<option value="leaf">Leaf (holds documents)</option>
					<option value="router">Router (holds stores)</option>
				</select></label
			>
			<label class="field"
				><span class="label">Parent</span>
				<select class="input" bind:value={form.parent_id}>
					<option value={null}>None (top level)</option>
					{#each routers as r}<option value={r.id}>{r.name}</option>{/each}
				</select></label
			>
			<label class="field"
				><span class="label">Description</span><input class="input" bind:value={form.description} /></label
			>
			<button class="btn" type="submit" disabled={!canWrite || !form.name.trim()} title={why}
				>Create store</button
			>
		</form>
	</Card>
	<Card title={sel ? `Documents in ${sel.name}` : 'Documents'}>
		{#if sel}
			<p class="muted">
				{sel.kind === 'router' ? 'Router store' : 'Leaf store'} · {storePath(sel)}{sel.description
					? ` · ${sel.description}`
					: ''}
			</p>
			<KeyValues
				pairs={[
					['Tokens', fmt.n(sel.token_count)],
					['Chunks', fmt.n(sel.chunk_count)],
				]}
			/>
			<div class="row">
				{#if sel.kind === 'router'}
					<label class="check"
						><input type="checkbox" bind:checked={recursive} /> With every store below it</label
					>
				{/if}
				<button
					class="btn sm"
					type="button"
					disabled={!canWrite}
					title={why}
					onclick={() => (confirmStore = true)}>Delete store</button
				>
			</div>
			{#if confirmStore}
				<div class="callout err" role="alertdialog" aria-label="Delete store">
					<strong>Delete {sel.name}?</strong>
					<span
						>{sel.kind === 'router' && recursive
							? 'Every store below it is deleted too, with all their documents.'
							: 'The store and its documents are deleted from strata_rag.'}</span
					>
					<span
						><button class="btn sm" type="button" onclick={removeStore}>Delete store</button>
						<button class="btn sm" type="button" onclick={() => (confirmStore = false)}>Keep it</button></span
					>
				</div>
			{/if}
		{/if}
		{#if confirmDoc}
			<div class="callout err" role="alertdialog" aria-label="Delete document">
				<strong>Delete {docName(confirmDoc)}?</strong>
				<span>It is removed from strata_rag and no longer found by search.</span>
				<span
					><button class="btn sm" type="button" onclick={() => removeDoc(confirmDoc!)}>Delete document</button
					>
					<button class="btn sm" type="button" onclick={() => (confirmDoc = null)}>Keep it</button></span
				>
			</div>
		{/if}
		{#if docs.s === 'idle'}
			<div class="empty">Pick a store to see its documents.</div>
		{:else if docs.s === 'loading'}
			<p class="muted">Loading…</p>
		{:else if docs.s === 'err'}
			<Unavailable reason={docs.err} retry={() => sel && pick(sel)} />
		{:else if docs.items.length === 0}
			<div class="empty">
				{sel?.kind === 'router'
					? 'A router holds no documents: it sends a query on to the stores below it.'
					: 'This store has no documents.'}
			</div>
		{:else}
			<ul class="list-plain">
				{#each docs.items as d}
					<li>
						<span
							>{docName(d)}{#if d.title && d.source}<span class="cell-sub mono">{d.source}</span>{/if}</span
						>
						<span class="muted mono">{fmt.n(d.token_count)} tokens · {fmt.n(d.chunk_count)} chunks</span>
						<button
							class="btn sm"
							type="button"
							disabled={!canWrite}
							title={why}
							onclick={() => (confirmDoc = d)}>Delete</button
						>
					</li>
				{/each}
			</ul>
			{#if docs.next}
				<button class="btn sm" type="button" disabled={loadingMore} onclick={more}
					>{loadingMore ? 'Loading…' : 'Load more'}</button
				>
			{/if}
		{/if}
	</Card>
</div>

<Card title="Search preview" sub="POST /api/knowledge/query → strata_rag /api/query">
	<form
		class="row"
		onsubmit={(e) => {
			e.preventDefault();
			run();
		}}
	>
		<input class="input" style="flex:1" bind:value={q} aria-label="Query" />
		<Seg
			items={[
				['embed', 'Embedding'],
				['laya', 'Laya routing'],
				['hybrid', 'Hybrid'],
			]}
			bind:active={mode}
		/>
		<label class="field">
			<span class="label">Results (top_k)</span>
			<input class="input" type="number" min="1" max="50" bind:value={topK} />
		</label>
		<label class="field" title="How many router branches the query follows at each level of the store tree">
			<span class="label">Branches (beam)</span>
			<input class="input" type="number" min="1" max="8" bind:value={beam} />
		</label>
		<label class="field" title="Passages scoring below this (0 to 1) are left out">
			<span class="label">Minimum score</span>
			<input class="input" type="number" min="0" max="1" step="0.05" bind:value={minScore} />
		</label>
		<button class="btn primary" type="submit" disabled={running}>{running ? 'Searching…' : 'Search'}</button>
	</form>
	{#if queryErr}
		<Unavailable reason={queryErr} />
	{/if}
	{#if result}
		<p class="muted">
			Mode {result.routing.mode_used} · searched {result.routing.leaves.map(storeName).join(', ') ||
				'no stores'} · laya
			{result.routing.laya_consulted ? 'consulted' : 'not consulted'}{result.routing.laya_fallback
				? ` (fallback: ${result.routing.laya_fallback})`
				: ''} · {fmt.ms(result.timings_ms.total)} total (embed {fmt.ms(result.timings_ms.embed)}, route {fmt.ms(
				result.timings_ms.route,
			)}, search {fmt.ms(result.timings_ms.search)}, rerank {fmt.ms(result.timings_ms.rerank)})
		</p>
		{#if result.results.length === 0}
			<div class="empty">
				Nothing in the knowledge stores matched closely enough. Only passages scoring at least {usedMinScore} are
				shown: try other words or lower the minimum score.
			</div>
		{/if}
		<ol class="list-plain">
			{#each result.results as r}
				<li>
					<span>
						<span class="mono">{r.store_path.join(' / ')}</span>
						<span class="cite" data-testid="hit-citation">{citation(r)}</span>
						<span class="cell-sub">{r.text}</span>
					</span>
					<span class="kn-scores mono muted">
						<span>dense {score(r.scores.dense)} <Meter value={r.scores.dense ?? 0} /></span>
						<span>lexical {score(r.scores.lexical)} <Meter value={r.scores.lexical ?? 0} cls="s-work" /></span
						>
						<span>rerank {score(r.scores.rerank)} <Meter value={r.scores.rerank ?? 0} cls="s-ok" /></span>
					</span>
				</li>
			{/each}
		</ol>
	{/if}
</Card>

<style>
	.cite {
		display: block;
		font-weight: 500;
	}
	.kn-node {
		all: unset;
		box-sizing: border-box;
		cursor: pointer;
		display: grid;
		grid-template-columns: auto minmax(0, 1fr) auto;
		gap: 8px;
		align-items: center;
		width: 100%;
		padding: 6px 8px 6px calc(8px + var(--d) * 18px);
		border-radius: 6px;
	}
	.kn-node:hover,
	.kn-node.on {
		background: var(--sunk);
	}
	.kn-node:focus-visible {
		outline: 2px solid var(--accent);
	}
	.new-store {
		margin-top: 12px;
	}
	.kn-scores {
		display: grid;
		gap: 2px;
		min-width: 150px;
	}
	.check {
		display: flex;
		gap: 6px;
		align-items: center;
	}
</style>
