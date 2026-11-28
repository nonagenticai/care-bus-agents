<script lang="ts">
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { setQuery } from '$lib/query';
	import { jobPath } from '$lib/scope';
	import Pagination from '$lib/ui/Pagination.svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { areaLabel, areasOf, caseProcedure, outcomeLabel, spaceLabels } from './lanes';
	import type { CaseRow, ContextSpace, Paged } from '$lib/api/types';
	import { CASE_FILTERS } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import FilterInput from '$lib/ui/FilterInput.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import SortTh from '$lib/ui/SortTh.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { debounce } from '$lib/debounce';
	import { nextSort, type SortState } from '$lib/sort';
	const PAGE = 50;
	const outcome = $derived(page.url.searchParams.get('outcome') ?? 'all');
	const area = $derived(page.url.searchParams.get('area') ?? '');
	const requestType = $derived(page.url.searchParams.get('request_type') ?? '');
	const before = $derived(page.url.searchParams.get('before') ?? '');
	const caseParam = $derived(page.url.searchParams.get('case') ?? '');
	// Search and order belong to the server: `filter` is what the box shows, `search` its debounced value sent.
	let filter = $state('');
	let search = $state('');
	let sort = $state<SortState | null>(null);
	const filtered = $derived(
		outcome !== 'all' || area !== '' || requestType !== '' || search !== '' || before !== '',
	);
	// `open` is filtered server-side (outcome IS NULL), so every page of results holds only what it shows.
	const list = new Resource<Paged<CaseRow>>(
		() =>
			api().cases({
				outcome: CASE_FILTERS.find((f) => f === outcome),
				area: area || undefined,
				request_type: requestType || undefined,
				q: search || undefined,
				sort: sort?.key,
				dir: sort?.dir,
				before: before || undefined,
				limit: PAGE,
			}),
		(p) => p.items.length === 0,
	);
	function reload() {
		list.value = { state: 'loading' };
		void list.refresh();
	}
	// The list is the server's: any filter, order or cursor change is a new request.
	$effect(() => {
		void outcome;
		void area;
		void requestType;
		void search;
		void sort?.key;
		void sort?.dir;
		void before;
		reload();
	});
	// Areas are the job's configuration: the areas of its context spaces. The active filter stays selectable
	// even when no space names it (a removed area, or the list has not loaded).
	const spaces = new Resource<Paged<ContextSpace>>(() => api().contextSpaces());
	spaces.refresh();
	const spaceItems = $derived(spaces.value.state === 'ok' ? spaces.value.data.items : []);
	const areaOptions = $derived(areasOf(spaceItems, area));
	// Context-space labels are a nicety: without them the page shows the raw keys.
	const labels = $derived(spaceLabels(spaceItems));
	const rows = $derived(list.value.state === 'ok' ? list.value.data.items : []);
	// `?case=<id>` deep-links (the console links here, and so does every bookmark of the old drawer). The case is
	// a page of its own now — `/cases/<id>` — so the deep link forwards instead of opening a drawer.
	const job = $derived(page.params.job ?? '');
	$effect(() => {
		const id = caseParam;
		if (id) void goto(jobPath(job, `/cases/${encodeURIComponent(id)}`), { replace: true });
	});

	// One keystroke is not one request: the box's value becomes the API's `q` after 300 ms, and at once when cleared.
	let seq = 0;
	const applySearch = debounce((v: string, n: number) => {
		if (n === seq) search = v;
	}, 300);
	$effect(() => {
		const v = filter;
		seq += 1;
		if (v.trim() === '') search = '';
		else applySearch(v, seq);
	});
	const toggleSort = (key: string) => {
		sort = nextSort(sort, key);
		// The cursor belongs to the old order: a new sort starts again from the top.
		if (before) void setQuery({ before: '', previous: '' });
	};
	const clearFilters = () => {
		filter = '';
		search = '';
		void setQuery({ outcome: '', area: '', request_type: '', before: '', previous: '' });
	};
	const openCase = (id: string) => void goto(jobPath(job, `/cases/${encodeURIComponent(id)}`));
	const get_outcome = () => outcome;
	const set_outcome = (v: string) => void setQuery({ outcome: v });
	const get_area = () => area;
	const set_area = (v: string) => void setQuery({ area: v });
</script>

<header class="page-head">
	<div>
		<h1>Cases</h1>
		<p>Every customer request that reached the bus, with its full trace.</p>
	</div>
	<div class="actions">
		<Freshness resource={list} />
		<!-- prettier-ignore -->
		<Seg
			items={[
				['all', 'All'],
				['completed', 'Completed'],
				['not_eligible', 'Not eligible'],
				['failed', 'Failed'],
				['open', 'Open'],
			]}
			bind:active={get_outcome, set_outcome}
		/>
		<label class="field">
			<span class="label">Area</span>
			<!-- prettier-ignore -->
			<select class="input" bind:value={get_area, set_area}>
				<option value="">All areas</option>
				{#each areaOptions as [k, l]}<option value={k}>{l}</option>{/each}
			</select>
		</label>
	</div>
</header>

{#if list.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if list.value.state === 'unavailable'}
	<Unavailable reason={list.value.reason} retry={reload} />
{:else if list.value.state === 'empty'}
	<div class="empty">
		{#if filtered}
			No cases match these filters.
			<button class="btn" onclick={clearFilters}>Clear filters</button>
		{:else}
			No cases yet. A case appears here when a customer request reaches the bus.
		{/if}
	</div>
{:else}
	<FilterInput bind:value={filter} label="Search cases" placeholder="Search case, customer or request" />
	{#if requestType}
		<button type="button" class="chip" onclick={() => setQuery({ request_type: '' })}>
			Request: {labels.type(requestType)} ×
		</button>
	{/if}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable stack-phone">
				<thead>
					<tr>
						<SortTh key="case" label="Case" {sort} onSort={toggleSort} />
						<SortTh key="area" label="Area" {sort} onSort={toggleSort} />
						<SortTh key="request" label="Request" {sort} onSort={toggleSort} />
						<SortTh key="outcome" label="Outcome" {sort} onSort={toggleSort} />
						<SortTh key="work" label="Work items" cls="r" {sort} onSort={toggleSort} />
						<SortTh key="time" label="Time" cls="r" {sort} onSort={toggleSort} />
					</tr>
				</thead>
				<tbody>
					{#each rows as c (c.case_id)}
						{@const proc = caseProcedure(c, labels.procedure)}
						<tr onclick={() => openCase(c.case_id)}>
							<td data-label="Case">
								<button
									type="button"
									class="table-action mono"
									onclick={(e) => {
										e.stopPropagation();
										openCase(c.case_id);
									}}
								>
									{c.case_id}
								</button><span class="cell-sub mono">{c.customer_id}</span>
							</td>
							<td data-label="Area">{areaLabel(c.area)}</td>
							<td data-label="Request">
								{labels.type(c.request_type)}{#if proc?.recorded}<span
										class="cell-sub"
										title="Procedure version this case ran"
									>
										procedure <span class="mono">{proc.text}</span>
									</span>{:else if proc}<span
										class="cell-sub"
										title="No version recorded for this case; this is the procedure its context space runs"
									>
										procedure <span class="mono">{proc.text}</span> (from context space)
									</span>{/if}
							</td>
							<td data-label="Outcome">
								<Status word={c.outcome ?? 'open'} label={outcomeLabel(c.outcome)} />
							</td>
							<td class="r" data-label="Work items">
								{c.work_items}{c.retries ? ` (+${c.retries} retry)` : ''}
							</td>
							<td class="r" data-label="Time">{fmt.ms(c.duration_ms)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

{#if list.value.state === 'ok' || (list.value.state === 'empty' && list.value.data.next)}<Pagination
		next={list.value.data.next}
		count={list.value.data.items.length}
		busy={list.refreshing}
	/>
{:else if before && list.value.state !== 'loading'}<Pagination
		escape
		next={null}
		count={0}
		busy={list.refreshing}
	/>{/if}
