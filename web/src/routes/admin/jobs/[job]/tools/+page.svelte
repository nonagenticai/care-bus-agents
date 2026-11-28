<script lang="ts">
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import type { Paged, ToolInfo } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import FilterInput from '$lib/ui/FilterInput.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import SortTh from '$lib/ui/SortTh.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { nextSort, sortRows, type SortState } from '$lib/sort';
	import { debounce } from '$lib/debounce';
	import { schemaFields } from './schema';

	let filter = $state('');
	let search = $state('');
	const pushSearch = debounce(() => (search = filter.trim()), 300);
	$effect(() => {
		// Clearing the box applies at once; typing waits out the debounce.
		if (filter.trim() === '') search = '';
		else pushSearch();
	});
	const tools = new Resource<Paged<ToolInfo>>(() => api().tools(search || undefined));
	$effect(() => {
		// The API filters the catalogue, so a new search reloads the list.
		void search;
		tools.refresh();
	});
	let open = $state(false);
	let sel = $state<ToolInfo | null>(null);
	let sort = $state<SortState | null>(null);
	const toggleSort = (key: string) => (sort = nextSort(sort, key));
	const group = (name: string) => name.split('_')[0] ?? name;
	const rows = $derived(tools.value.state === 'ok' ? tools.value.data.items : []);
	const shown = $derived(
		sortRows(rows, sort, (t, key) => {
			switch (key) {
				case 'tool':
					return t.name;
				case 'group':
					return group(t.name);
				case 'calls':
					return t.calls_24h;
				case 'errors':
					return t.error_rate;
				case 'p95':
					return t.p95_ms;
				default:
					return null;
			}
		}),
	);

	function select(t: ToolInfo) {
		sel = t;
		open = true;
	}

	function clearSearch() {
		filter = '';
		search = '';
	}
</script>

<header class="page-head">
	<div>
		<h1>MCP tools</h1>
		<p>Tools served by nonagenticai_mcp_server. Call counts come from carebus audit rows.</p>
	</div>
	<div class="actions"><Freshness resource={tools} /></div>
</header>

{#if tools.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if tools.value.state === 'unavailable'}
	<Unavailable reason={tools.value.reason} retry={() => tools.refresh()} />
{:else if tools.value.state === 'empty' && !search}
	<div class="empty">
		The MCP server lists no tools for this job. Tools come from the job's release and MCP bridge.
	</div>
{:else}
	<FilterInput bind:value={filter} label="Search tools" placeholder="Search name or description" />
	{#if tools.value.state === 'empty'}
		<div class="empty">
			No tools match “{search}”.
			<button class="btn" onclick={clearSearch}>Clear search</button>
		</div>
	{:else}
		<Card cls="flush">
			<div class="tbl-wrap">
				<table class="tbl clickable stack-phone">
					<thead>
						<tr>
							<SortTh key="tool" label="Tool" {sort} onSort={toggleSort} />
							<SortTh key="group" label="Group" {sort} onSort={toggleSort} />
							<th>Description</th>
							<SortTh key="calls" label="Calls 24h" cls="r" {sort} onSort={toggleSort} />
							<SortTh key="errors" label="Error rate" cls="r" {sort} onSort={toggleSort} />
							<SortTh key="p95" label="p95" cls="r" {sort} onSort={toggleSort} />
						</tr>
					</thead>
					<tbody>
						{#each shown as t}
							<tr>
								<td class="mono" data-label="Tool">
									<button type="button" class="table-action" onclick={() => select(t)}>{t.name}</button>
								</td>
								<td data-label="Group"><Chip text={group(t.name)} variant="tool" /></td>
								<td data-label="Description">{t.description ?? 'No description'}</td>
								<td class="r" data-label="Calls 24h">{fmt.n(t.calls_24h)}</td>
								<td class="r" data-label="Error rate">{fmt.pct(t.error_rate)}</td>
								<td class="r" data-label="p95">{fmt.ms(t.p95_ms)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/if}
{/if}

<Drawer bind:open title={sel?.name ?? ''} wide>
	{#if sel}
		<p>{sel.description ?? 'No description'}</p>
		{@const fields = schemaFields(sel.input_schema)}
		<h3>Inputs</h3>
		{#if fields.length}
			<div class="tbl-wrap">
				<table class="tbl dense stack-phone">
					<thead><tr><th>Name</th><th>Type</th><th>Required</th><th>Description</th></tr></thead>
					<tbody>
						{#each fields as f (f.name)}
							<tr>
								<td class="mono" data-label="Name">{f.name}</td>
								<td class="mono" data-label="Type">{f.type}</td>
								<td data-label="Required">{f.required ? 'Required' : 'Optional'}</td>
								<td data-label="Description">{f.description || '—'}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else}
			<p class="muted">This tool takes no named inputs.</p>
		{/if}
		<details>
			<summary>Input schema as JSON</summary>
			<JsonView value={sel.input_schema} />
		</details>
	{/if}
</Drawer>
