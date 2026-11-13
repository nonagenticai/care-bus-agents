<script lang="ts">
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import type { Paged, ToolInfo } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { schemaFields } from './schema';

	const tools = new Resource<Paged<ToolInfo>>(() => api().tools());
	tools.refresh();
	let open = $state(false);
	let sel = $state<ToolInfo | null>(null);
	const group = (name: string) => name.split('_')[0] ?? name;

	function select(t: ToolInfo) {
		sel = t;
		open = true;
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
{:else if tools.value.state === 'empty'}
	<div class="empty">The MCP server lists no tools.</div>
{:else}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead>
					<tr
						><th>Tool</th><th>Group</th><th>Description</th><th class="r">Calls 24h</th><th class="r"
							>Error rate</th
						><th class="r">p95</th></tr
					>
				</thead>
				<tbody>
					{#each tools.value.data.items as t}
						<tr
							><td class="mono"
								><button type="button" class="table-action" onclick={() => select(t)}>{t.name}</button></td
							>
							<td><Chip text={group(t.name)} variant="tool" /></td>
							<td>{t.description ?? 'No description'}</td>
							<td class="r">{fmt.n(t.calls_24h)}</td>
							<td class="r">{fmt.pct(t.error_rate)}</td>
							<td class="r">{fmt.ms(t.p95_ms)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

<Drawer bind:open title={sel?.name ?? ''} wide>
	{#if sel}
		<p>{sel.description ?? 'No description'}</p>
		{@const fields = schemaFields(sel.input_schema)}
		<h3>Inputs</h3>
		{#if fields.length}
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead><tr><th>Name</th><th>Type</th><th>Required</th><th>Description</th></tr></thead>
					<tbody>
						{#each fields as f (f.name)}
							<tr>
								<td class="mono">{f.name}</td>
								<td class="mono">{f.type}</td>
								<td>{f.required ? 'Required' : 'Optional'}</td>
								<td>{f.description || '—'}</td>
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
