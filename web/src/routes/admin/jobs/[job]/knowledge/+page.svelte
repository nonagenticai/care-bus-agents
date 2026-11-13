<script lang="ts">
	import Freshness from '$lib/ui/Freshness.svelte';
	import { page } from '$app/state';
	import Pagination from '$lib/ui/Pagination.svelte';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { RUN_LABELS, SOURCE_TYPES } from '$lib/sources';
	import type { OntologyEntity, Paged, Source, StoreNode } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import SourceEditor from './SourceEditor.svelte';
	import RunHistory from './RunHistory.svelte';

	const before = $derived(page.url.searchParams.get('before') ?? '');
	const list = new Resource<Paged<Source>>(() => api().sources({ limit: 200, before }));
	const stores = new Resource<Paged<StoreNode>>(() => api().stores());
	const entities = new Resource<Paged<OntologyEntity>>(() => api().entities());
	// Keep the old rows visible while the next page loads (Resource.refreshing), no reset to 'loading'.
	$effect(() => {
		void before;
		list.refresh();
	});
	stores.refresh();
	entities.refresh();
	const canWrite = $derived(session.can('tenant_admin'));
	let open = $state(false);
	let editorDirty = $state(false);
	let current = $state<Source | null>(null);
	const storeItems = $derived(stores.value.state === 'ok' ? stores.value.data.items : []);
	const entityItems = $derived(entities.value.state === 'ok' ? entities.value.data.items : []);
	const storeName = (id: string) => storeItems.find((s) => s.id === id)?.name ?? id.slice(0, 8);
	const typeLabel = (t: string) => SOURCE_TYPES.find(([k]) => k === t)?.[1] ?? t;
	function edit(s: Source | null) {
		current = s;
		open = true;
	}
	function saved(s: Source | null) {
		if (s) current = s;
		else open = false;
		list.refresh();
	}
</script>

<header class="page-head">
	<div>
		<h1>Sources</h1>
		<p>
			Where customer knowledge comes from. Supported sources sync through the ingest service; completed
			indexing makes documents available in a knowledge store.
		</p>
	</div>
	<div class="actions">
		<a class="btn" href="/admin/knowledge/stores">Stores</a>
		<a class="btn" href="/admin/knowledge/jobs">Ingest jobs</a>
		<button
			class="btn primary"
			type="button"
			disabled={!canWrite}
			title={canWrite ? '' : 'Needs the tenant_admin role'}
			onclick={() => edit(null)}>Add source</button
		>
	</div>
</header>
<Freshness resource={list} />

{#if stores.value.state === 'unavailable'}
	<Unavailable reason={`Stores: ${stores.value.reason}`} retry={() => stores.refresh()} />
{/if}
{#if list.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if list.value.state === 'unavailable'}
	<Unavailable reason={list.value.reason} retry={() => list.refresh()} />
{:else if list.value.state === 'empty'}
	<div class="empty">No sources yet. Add a website, API, file or datatable source.</div>
{:else}
	{#if list.value.data.next}
		<p class="muted" role="status">More sources are available using Older results below.</p>
	{/if}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead
					><tr
						><th>Name</th><th>Type</th><th>Target store</th><th>Last run</th><th class="r">Documents</th><th
							>Enabled</th
						></tr
					></thead
				>
				<tbody>
					{#each list.value.data.items as s}
						<tr
							><td><button type="button" class="table-action" onclick={() => edit(s)}>{s.name}</button></td>
							<td>{typeLabel(s.type)}</td>
							<td class="mono">{storeName(s.target_store_id)}</td>
							<td>
								{#if s.last_run}
									<Status word={s.last_run.status} label={RUN_LABELS[s.last_run.status]} />
									{#if s.last_run.error}<span class="cell-sub">{s.last_run.error}</span>{/if}
								{:else}
									<span class="muted">Not synced yet</span>
								{/if}
							</td>
							<td class="r">{s.last_run ? fmt.n(s.last_run.document_count) : '—'}</td>
							<td>{s.enabled ? 'Yes' : 'No'}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

{#if list.value.state === 'ok' || list.value.state === 'empty'}
	<Pagination next={list.value.data.next} count={list.value.data.items.length} busy={list.refreshing} />
{/if}
<Drawer bind:open title={current?.name ?? 'Add source'} wide dirty={editorDirty}>
	<SourceEditor
		bind:dirty={editorDirty}
		source={current}
		stores={storeItems}
		entities={entityItems}
		{canWrite}
		onsaved={saved}
	/>
	{#if current}
		<RunHistory source={current} {canWrite} onchange={() => list.refresh()} />
	{/if}
</Drawer>
