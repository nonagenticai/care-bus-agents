<script lang="ts">
	import { untrack } from 'svelte';
	import { page } from '$app/state';
	import { setQuery } from '$lib/query';
	import Pagination from '$lib/ui/Pagination.svelte';
	import { api } from '$lib/app.svelte';
	import { Latest, Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { summary } from '$lib/console/bus';
	import {
		agentName,
		areaLabel,
		areasOf,
		caseProcedure,
		outcomeLabel,
		spaceLabels,
		traceRows,
	} from './lanes';
	import type { CaseDetail, CaseRow, ContextSpace, Paged } from '$lib/api/types';
	import { CASE_FILTERS } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
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
	// `?case=<id>` deep-links (the console links here). React only when the value itself changes.
	$effect(() => {
		const id = caseParam;
		untrack(() => {
			if (id) {
				if (!(open && detailId === id)) void show(id);
			} else open = false;
		});
	});
	// Closing the drawer removes `case` from the URL.
	$effect(() => {
		if (!open && untrack(() => caseParam)) void setQuery({ case: '' }, { resetPage: false });
	});

	const get_outcome = () => outcome;
	const set_outcome = (v: string) => void setQuery({ outcome: v });
	const get_area = () => area;
	const set_area = (v: string) => void setQuery({ area: v });
	let open = $state(false);
	let detailId = $state('');
	let detail = $state<CaseDetail | null>(null);
	let detailError = $state('');
	const latestDetail = new Latest();
	async function show(id: string) {
		const ticket = latestDetail.next();
		open = true;
		detailId = id;
		detail = null;
		detailError = '';
		try {
			const d = await api().case(id);
			if (latestDetail.stale(ticket)) return;
			detail = d;
		} catch (e) {
			if (latestDetail.stale(ticket)) return;
			detailError = (e as Error).message;
		}
	}
	const when = (iso: string | null) => (iso ? iso.replace('T', ' ').slice(0, 19) : '—');
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
			<button class="btn" onclick={() => setQuery({ outcome: '', area: '', before: '', previous: '' })}
				>Clear filters</button
			>
		{:else}
			No cases yet.
		{/if}
	</div>
{:else}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead>
					<tr
						><th>Case</th><th>Area</th><th>Request</th><th>Outcome</th><th class="r">Work items</th><th
							class="r">Time</th
						></tr
					>
				</thead>
				<tbody>
					{#each rows as c (c.case_id)}
						{@const proc = caseProcedure(c, labels.procedure)}
						<tr onclick={() => setQuery({ case: c.case_id }, { resetPage: false })}>
							<td
								><button
									type="button"
									class="table-action mono"
									onclick={(e) => {
										e.stopPropagation();
										void setQuery({ case: c.case_id }, { resetPage: false });
									}}>{c.case_id}</button
								><span class="cell-sub mono">{c.customer_id}</span></td
							>
							<td>{areaLabel(c.area)}</td>
							<td
								>{labels.type(c.request_type)}{#if proc?.recorded}<span
										class="cell-sub"
										title="Procedure version this case ran"
										>procedure <span class="mono">{proc.text}</span></span
									>{:else if proc}<span
										class="cell-sub"
										title="No version recorded for this case; this is the procedure its context space runs"
										>procedure <span class="mono">{proc.text}</span> (from context space)</span
									>{/if}</td
							>
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
<Drawer bind:open title={detail?.case.case_id ?? (detailId || 'Case')} wide>
	{#if detailError}
		<Unavailable reason={detailError} retry={() => show(detailId)} />
	{:else if !detail}
		<p class="muted">Loading…</p>
	{:else}
		{@const c = detail.case}
		{@const proc = caseProcedure(c, labels.procedure)}
		{#if c.reason}
			<Callout kind={c.outcome === 'failed' ? 'err' : 'info'} title={outcomeLabel(c.outcome)}
				>{c.reason}</Callout
			>
		{/if}
		<KeyValues
			pairs={[
				['Request', labels.type(c.request_type)],
				['Context space', c.context_space],
				[
					'Procedure',
					proc?.recorded ? proc.text : proc ? `${proc.text} (from context space; version not recorded)` : '—',
				],
				['Area', areaLabel(c.area)],
				['Outcome', outcomeLabel(c.outcome)],
				['Customer', c.customer_id],
				['Conversation', c.conversation_id ?? '—'],
				['Work items', `${c.work_items}${c.retries ? ` (+${c.retries} retry)` : ''}`],
				['Total time', fmt.ms(c.duration_ms)],
				['Started', when(c.started_at)],
				['Resolved', when(c.resolved_at)],
			]}
		/>
		<h3>Trace across the buses</h3>
		<div class="ca-seq-wrap">
			<div class="ca-seq">
				<div class="ca-seq-h">
					<span>Time</span><span>customer-bus</span><span>worker-bus</span><span>tool-bus</span>
				</div>
				{#each traceRows(detail.trace) as r}
					{@const note = summary(r.ev)}
					<div class="ca-row">
						<span class="ca-t">+{fmt.ms(r.t_ms)}</span>
						{#each ['customer', 'worker', 'tool'] as lane}
							<span class="ca-lane">
								{#if r.lane === lane}
									<span
										class="ca-ev"
										style="--c:var(--{lane === 'customer' ? 'cust' : lane === 'worker' ? 'work' : 'tool'})"
									>
										<b>{r.ev.envelope.topic.split('.').slice(-2).join('.')}</b>
										<span title={r.ev.envelope.from}>{agentName(r.ev.envelope.from)}</span>
										{#if note}<span class="mono">{note}</span>{/if}
									</span>
								{/if}
							</span>
						{/each}
					</div>
				{/each}
			</div>
		</div>
		<h3>Case hash</h3>
		<JsonView value={detail.hash} />
	{/if}
</Drawer>

<style>
	.ca-seq-wrap {
		overflow-x: auto;
	}
	.ca-seq {
		display: grid;
		min-width: 640px;
		font-size: 12px;
	}
	.ca-seq-h,
	.ca-row {
		display: grid;
		grid-template-columns: 64px repeat(3, minmax(0, 1fr));
		gap: 8px;
	}
	.ca-seq-h {
		padding-bottom: 6px;
		overflow-wrap: anywhere;
		border-bottom: 1px solid var(--line);
		font: 600 11px var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.ca-row {
		padding: 3px 0;
		position: relative;
	}
	.ca-t {
		color: var(--faint);
		padding-top: 5px;
		font-size: 11px;
	}
	.ca-lane {
		border-left: 1px dashed var(--line);
		margin-left: 10px;
	}
	.ca-ev {
		display: grid;
		gap: 1px;
		padding: 5px 8px;
		border-radius: 5px;
		border-left: 3px solid var(--c);
		background: color-mix(in srgb, var(--c) 8%, var(--panel));
		min-width: 0;
		--c: var(--cust);
	}
	.ca-ev b {
		color: var(--c);
		font-weight: 500;
		font-size: 12px;
	}
	.ca-ev .mono {
		font-size: 11px;
		overflow-wrap: anywhere;
	}
</style>
