<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { debounce } from '$lib/debounce';
	import { fmt } from '$lib/format';
	import { cellText, isDemoTable, PAGE_SIZE, rowKey, tableLabel } from '../tables';
	import type { DemoRecord, DemoTablePage } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const table = $derived(page.params.table ?? '');
	const label = $derived(tableLabel(table));
	let q = $state('');
	let offset = $state(0);

	const rows = new Resource<DemoTablePage>(
		async () => {
			if (!isDemoTable(table)) throw new Error(`Unknown demo table "${table}"`);
			return api().demoRecords(table, { q: q || undefined, limit: PAGE_SIZE, offset });
		},
		(p) => p.rows.length === 0,
	);
	const soon = debounce(() => rows.refresh(), 250);
	$effect(() => {
		void table;
		void q;
		offset = 0;
		soon();
	});

	function go(to: number) {
		offset = Math.max(0, to);
		rows.refresh();
	}
	function open(p: DemoTablePage, r: DemoRecord) {
		void goto(`/demo/${table}/${encodeURIComponent(rowKey(p, r))}`);
	}
</script>

<header class="page-head">
	<div>
		<h1>{label}</h1>
		<p><a href="/demo">Demo data</a> / {label}</p>
	</div>
	<input
		class="input"
		style="max-width:280px"
		bind:value={q}
		placeholder="Search"
		aria-label="Search records"
	/>
</header>

{#if rows.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if rows.value.state === 'unavailable'}
	<Unavailable reason={rows.value.reason} retry={() => rows.refresh()} />
{:else if rows.value.state === 'empty'}
	<div class="empty">
		No records match.
		{#if offset > 0}<button class="btn sm" type="button" onclick={() => go(offset - PAGE_SIZE)}
				>Previous</button
			>{/if}
	</div>
{:else if rows.value.state === 'ok'}
	{@const p = rows.value.data}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable dense">
				<thead>
					<tr>
						{#each p.columns as c (c)}
							<th
								>{c}{#if c === p.primary_key}
									<span class="chip">key</span>{/if}</th
							>
						{/each}
					</tr>
				</thead>
				<tbody>
					{#each p.rows as r (rowKey(p, r))}
						<tr tabindex="0" onclick={() => open(p, r)} onkeydown={(e) => e.key === 'Enter' && open(p, r)}>
							{#each p.columns as c (c)}
								<td class:mono={c.endsWith('_id') || c === p.primary_key}>{cellText(r[c])}</td>
							{/each}
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
	<div class="split">
		<span class="muted">{fmt.n(offset + 1)}–{fmt.n(offset + p.rows.length)} of {fmt.n(p.total)}</span>
		<div class="actions">
			<button class="btn sm" type="button" disabled={offset === 0} onclick={() => go(offset - PAGE_SIZE)}
				>Previous</button
			>
			<button
				class="btn sm"
				type="button"
				disabled={offset + p.rows.length >= p.total}
				onclick={() => go(offset + PAGE_SIZE)}>Next</button
			>
		</div>
	</div>
{/if}
