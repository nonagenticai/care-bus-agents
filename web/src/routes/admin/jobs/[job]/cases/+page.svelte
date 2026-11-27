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
	import Seg from '$lib/ui/Seg.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const PAGE = 50;
	const outcome = $derived(page.url.searchParams.get('outcome') ?? 'all');
	const area = $derived(page.url.searchParams.get('area') ?? '');
	const before = $derived(page.url.searchParams.get('before') ?? '');
	const caseParam = $derived(page.url.searchParams.get('case') ?? '');
	const filtered = $derived(outcome !== 'all' || area !== '' || before !== '');
	// `open` is filtered server-side (outcome IS NULL), so every page of results holds only what it shows.
	const list = new Resource<Paged<CaseRow>>(
		() =>
			api().cases({
				outcome: CASE_FILTERS.find((f) => f === outcome),
				area: area || undefined,
				before: before || undefined,
				limit: PAGE,
			}),
		(p) => p.items.length === 0,
	);
	function reload() {
		list.value = { state: 'loading' };
		void list.refresh();
	}
	$effect(() => {
		void outcome;
		void area;
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
			<button class="btn" onclick={() => setQuery({ outcome: '', area: '', before: '', previous: '' })}>
				Clear filters
			</button>
		{:else}
			No cases yet.
		{/if}
	</div>
{:else}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead>
					<tr>
						<th>Case</th><th>Area</th><th>Request</th><th>Outcome</th><th class="r">Work items</th><th
							class="r"
						>
							Time
						</th>
					</tr>
				</thead>
				<tbody>
					{#each rows as c (c.case_id)}
						{@const proc = caseProcedure(c, labels.procedure)}
						<tr onclick={() => openCase(c.case_id)}>
							<td>
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
							<td>{areaLabel(c.area)}</td>
							<td>
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
							<td><Status word={c.outcome ?? 'open'} label={outcomeLabel(c.outcome)} /></td>
							<td class="r">{c.work_items}{c.retries ? ` (+${c.retries} retry)` : ''}</td>
							<td class="r">{fmt.ms(c.duration_ms)}</td>
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
