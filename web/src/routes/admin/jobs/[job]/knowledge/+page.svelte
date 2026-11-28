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
	import FilterInput from '$lib/ui/FilterInput.svelte';
	import SortTh from '$lib/ui/SortTh.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { matchesQuery, nextSort, sortRows, type SortState } from '$lib/sort';
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
	// Client-side over the loaded page (the API pages by `before`, it does not search).
	let filter = $state('');
	let sort = $state<SortState | null>(null);
	const toggleSort = (key: string) => (sort = nextSort(sort, key));
	const rows = $derived(list.value.state === 'ok' ? list.value.data.items : []);
	const shown = $derived(
		sortRows(
			rows.filter((s) => matchesQuery(filter, [s.name, typeLabel(s.type), storeName(s.target_store_id)])),
			sort,
			(s, key) => {
				switch (key) {
					case 'name':
						return s.name;
					case 'type':
						return typeLabel(s.type);
					case 'store':
						return storeName(s.target_store_id);
					case 'documents':
						return s.last_run?.document_count ?? null;
					case 'enabled':
						return s.enabled ? 1 : 0;
					default:
						return null;
				}
			},
		),
	);
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
			onclick={() => edit(null)}
		>
			Add source
		</button>
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
	<FilterInput
		bind:value={filter}
		label="Search sources"
		placeholder="Search name, type or store"
		count={filter ? `${shown.length} of ${rows.length} loaded` : ''}
	/>
	{#if filter && shown.length === 0}
		<div class="empty">
			No loaded sources match “{filter}”.
			<button class="btn" onclick={() => (filter = '')}>Clear search</button>
		</div>
	{:else}
		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl clickable stack-phone">
					<thead>
						<tr>
							<SortTh key="name" label="Name" {sort} onSort={toggleSort} />
							<SortTh key="type" label="Type" {sort} onSort={toggleSort} />
							<SortTh key="store" label="Target store" {sort} onSort={toggleSort} />
							<th>Last run</th>
							<SortTh key="documents" label="Documents" cls="r" {sort} onSort={toggleSort} />
							<SortTh key="enabled" label="Enabled" {sort} onSort={toggleSort} />
						</tr>
					</thead>
					<tbody>
						{#each shown as s}
							<tr>
								<td data-label="Name">
									<button type="button" class="table-action" onclick={() => edit(s)}>{s.name}</button>
								</td>
								<td data-label="Type">{typeLabel(s.type)}</td>
								<td class="mono" data-label="Target store">{storeName(s.target_store_id)}</td>
								<td data-label="Last run">
									{#if s.last_run}
										<Status word={s.last_run.status} label={RUN_LABELS[s.last_run.status]} />
										{#if s.last_run.error}<span class="cell-sub">{s.last_run.error}</span>{/if}
									{:else}
										<span class="muted">Not synced yet</span>
									{/if}
								</td>
								<td class="r" data-label="Documents">
									{s.last_run ? fmt.n(s.last_run.document_count) : '—'}
								</td>
								<td data-label="Enabled">{s.enabled ? 'Yes' : 'No'}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/if}
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
