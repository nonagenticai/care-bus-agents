<script lang="ts">
	import { untrack } from 'svelte';
	import { page } from '$app/state';
	import { setQuery } from '$lib/query';
	import Pagination from '$lib/ui/Pagination.svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { debounce } from '$lib/debounce';
	import type { AuditRow, Paged } from '$lib/api/types';
	import { ACTOR_TYPES, RESOURCE_TYPES, actorTypeLabel, auditSummary, resourceTypeLabel } from './labels';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const PAGE = 100;
	const q = $derived(page.url.searchParams.get('q') ?? '');
	const actorType = $derived(page.url.searchParams.get('actor_type') ?? '');
	const status = $derived(page.url.searchParams.get('status') ?? '');
	const resourceType = $derived(page.url.searchParams.get('resource_type') ?? '');
	const before = $derived(page.url.searchParams.get('before') ?? '');
	const filtered = $derived(
		q !== '' || actorType !== '' || status !== '' || resourceType !== '' || before !== '',
	);
	// The search box keeps its own text; the URL (and so the request) follows it after a pause.
	let text = $state(untrack(() => q));
	$effect(() => {
		text = q;
	});
	const pushText = debounce((v: string) => void setQuery({ q: v }), 250);
	const query = () => ({
		q: q || undefined,
		actor_type: actorType || undefined,
		status: status || undefined,
		resource_type: resourceType || undefined,
		before: before || undefined,
		limit: PAGE,
	});

	const res = new Resource<Paged<AuditRow>>(
		() => api().audit(query()),
		(p) => p.items.length === 0,
	);
	const view = $derived(res.value);
	// A filter change starts from the loading state; Freshness refresh keeps the rows on screen.
	function run() {
		res.value = { state: 'loading' };
		void res.refresh();
	}
	$effect(() => {
		void q;
		void actorType;
		void status;
		void resourceType;
		void before;
		run();
	});

	let open = $state(false);
	let sel = $state<AuditRow | null>(null);
	const when = (iso: string) => iso.replace('T', ' ').slice(0, 19);
	function select(r: AuditRow) {
		sel = r;
		open = true;
	}
	const get_text = () => text;
	const set_text = (v: string) => {
		text = v;
		pushText(v);
	};
	const get_actorType = () => actorType;
	const set_actorType = (v: string) => void setQuery({ actor_type: v });
	const get_status = () => status;
	const set_status = (v: string) => void setQuery({ status: v });
	const get_resourceType = () => resourceType;
	const set_resourceType = (v: string) => void setQuery({ resource_type: v });
	function clear() {
		text = '';
		void setQuery({ q: '', actor_type: '', status: '', resource_type: '' });
	}
</script>

<header class="page-head">
	<div>
		<h1>Audit log</h1>
		<p>Admin changes and tool calls, newest first.</p>
	</div>
	<div class="actions"><Freshness resource={res} /></div>
</header>

<Card cls="flush">
	<div class="row" style="padding:12px 16px">
		<!-- prettier-ignore -->
		<input
			class="input"
			style="flex:1;min-width:180px"
			bind:value={get_text, set_text}
			placeholder="Search actor, action or resource"
			aria-label="Search audit log"
		/>
		<!-- prettier-ignore -->
		<select
			class="input"
			style="width:auto"
			bind:value={get_actorType, set_actorType}
			aria-label="Actor type"
		>
			<option value="">All actors</option>
			{#each ACTOR_TYPES as [k, l]}<option value={k}>{l}</option>{/each}
		</select>
		<!-- prettier-ignore -->
		<select class="input" style="width:auto" bind:value={get_status, set_status} aria-label="Status">
			<option value="">All statuses</option>
			<option value="success">Success</option>
			<option value="failure">Failure</option>
		</select>
		<!-- prettier-ignore -->
		<select
			class="input"
			style="width:auto"
			bind:value={get_resourceType, set_resourceType}
			aria-label="Resource type"
		>
			<option value="">All resources</option>
			{#each RESOURCE_TYPES as [k, l]}<option value={k}>{l}</option>{/each}
		</select>
		{#if filtered}<button class="btn sm" type="button" onclick={clear}>Clear filters</button>{/if}
	</div>

	{#if view.state === 'loading'}
		<p class="muted" style="padding:0 16px 16px">Loading…</p>
	{:else if view.state === 'unavailable'}
		<div style="padding:0 16px 16px">
			<Unavailable reason={view.reason} retry={res.retry} />
		</div>
	{:else if view.state === 'empty'}
		<div class="empty" style="padding:0 16px 16px">
			{filtered ? 'No audit rows match these filters.' : 'No audit rows yet.'}
		</div>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl clickable dense">
				<thead>
					<tr><th>When</th><th>Actor</th><th>Action</th><th>Resource</th><th>Status</th></tr>
				</thead>
				<tbody>
					{#each view.state === 'ok' ? view.data.items : [] as r (r.id)}
						<tr onclick={() => select(r)}>
							<td
								><button
									type="button"
									class="table-action mono"
									onclick={(e) => {
										e.stopPropagation();
										select(r);
									}}>{when(r.timestamp)}</button
								></td
							>
							<td>{actorTypeLabel(r.actor_type)}<span class="cell-sub mono">{r.actor_id}</span></td>
							<td class="mono">{r.action_type}</td>
							<td>{resourceTypeLabel(r.resource_type)}<span class="cell-sub mono">{r.resource_id}</span></td>
							<td><Status word={r.status} label={r.status === 'success' ? 'Success' : 'Failure'} /></td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</Card>

{#if view.state === 'ok' || (view.state === 'empty' && view.data.next)}<Pagination
		next={view.data.next}
		count={view.data.items.length}
		busy={res.refreshing}
	/>
{:else if before && view.state !== 'loading'}<Pagination escape next={null} count={0} />{/if}
<Drawer bind:open title={sel ? `${sel.action_type} · ${resourceTypeLabel(sel.resource_type)}` : ''}>
	{#if sel}
		<KeyValues pairs={auditSummary(sel)} />
		<h3>Details</h3>
		{#if Object.keys(sel.details ?? {}).length}<JsonView value={sel.details} />{:else}<p class="muted">
				No details recorded.
			</p>{/if}
	{/if}
</Drawer>
