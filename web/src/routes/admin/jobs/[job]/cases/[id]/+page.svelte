<script lang="ts">
	// One case, full page.
	//
	// The drawer this replaces put the bus trace in the middle third of an 820px panel and ended in a
	// 54,781-character JSON blob, so the two things a human opens a case for — what the agent told the customer,
	// and what the buses did — were the hardest parts to read. Here the trace takes the height that is left,
	// the bands above and below it collapse, and the raw hash is closed until asked for.
	import { page } from '$app/state';
	import { goto } from '$app/navigation';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { jobPath } from '$lib/scope';
	import { summary } from '$lib/console/bus';
	import { laneOf } from '$lib/console/console.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import EventDetail from '$lib/console/EventDetail.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { agentName, caseAnswer, caseProcedure, outcomeLabel, spaceLabels, traceRows } from '../lanes';
	import type { BusEvent, CaseDetail, ContextSpace, Paged } from '$lib/api/types';

	const job = $derived(page.params.job ?? '');
	const id = $derived(page.params.id ?? '');
	const back = $derived(jobPath(job, '/cases'));

	const detail = new Resource<CaseDetail>(() => api().case(id));
	const spaces = new Resource<Paged<ContextSpace>>(
		() => api().contextSpaces(),
		(p) => p.items.length === 0,
	);
	$effect(() => {
		void id;
		void job;
		void detail.refresh();
		void spaces.refresh();
	});

	const d = $derived(detail.value.state === 'ok' ? detail.value.data : null);
	const labels = $derived(spaceLabels(spaces.value.state === 'ok' ? spaces.value.data.items : []));
	const c = $derived(d?.case ?? null);
	const answer = $derived(d ? caseAnswer(d.hash) : null);
	const rows = $derived(d ? traceRows(d.trace) : []);
	const proc = $derived(c ? caseProcedure(c, labels.procedure) : null);

	// One row open at a time; the key is the stream entry, which is unique across lanes.
	const key = (e: BusEvent) => `${e.stream}|${e.entry_id}`;
	let openKey = $state('');
	const selected = $derived(rows.find((r) => key(r.ev) === openKey)?.ev ?? null);
	// Focus hides the bands and leaves the trace the whole page; the bands are back with one click.
	let focus = $state(false);

	const when = (iso: string | null) => (iso ? iso.replace('T', ' ').slice(0, 19) : '—');
</script>

{#if detail.value.state === 'loading' || spaces.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if detail.value.state === 'unavailable'}
	<Unavailable reason={detail.value.reason} retry={() => detail.refresh()} />
{:else if c}
	<div class="case-page" class:focus>
		<header class="case-head">
			<div class="ident">
				<a class="back" href={back}>← Cases</a>
				<h1 class="mono">{c.case_id}</h1>
				<div class="tags">
					<Status word={c.outcome ?? 'open'} label={outcomeLabel(c.outcome)} />
					{#if proc}<span class="chip accent mono">
							{proc.text}{proc.recorded ? '' : ' (from context space)'}
						</span>{/if}
					<span class="chip mono">{fmt.ms(c.duration_ms)}</span>
					<span class="chip">{labels.type(c.request_type)}</span>
				</div>
			</div>
			<div class="head-actions">
				<button class="btn sm" type="button" onclick={() => (focus = !focus)}>
					{focus ? 'Show summary' : 'Focus trace'}
				</button>
				<Freshness resource={detail} />
			</div>
		</header>

		{#if !focus}
			{#if c.reason}
				<Callout kind={c.outcome === 'failed' ? 'err' : 'info'} title={outcomeLabel(c.outcome)}>
					{c.reason}
				</Callout>
			{/if}
			<div class="bands">
				<details class="band">
					<summary>Summary <span class="hint">customer, timing, procedure</span></summary>
					<KeyValues
						pairs={[
							['Request', labels.type(c.request_type)],
							['Context space', c.context_space],
							[
								'Procedure',
								proc?.recorded
									? proc.text
									: proc
										? `${proc.text} (from context space; version not recorded)`
										: '—',
							],
							['Area', c.area],
							['Outcome', outcomeLabel(c.outcome)],
							['Customer', c.customer_id],
							['Conversation', c.conversation_id ?? '—'],
							['Work items', `${c.work_items}${c.retries ? ` (+${c.retries} retry)` : ''}`],
							['Total time', fmt.ms(c.duration_ms)],
							['Started', when(c.started_at)],
							['Resolved', when(c.resolved_at)],
						]}
					/>
				</details>
				<details class="band" open={!!answer}>
					<summary>
						Answer
						{#if answer}<span class="hint">
								{answer.cited
									? `cited ${answer.cited} source${answer.cited === 1 ? '' : 's'}`
									: 'no grounding'}
							</span>{:else}<span class="hint">not recorded for this request type</span>{/if}
					</summary>
					{#if answer}
						<p class="answer">{answer.text}</p>
					{:else}
						<p class="muted">
							This case has no stored answer — only the request types that produce one (the website FAQ, the
							assistant) record it in their facts.
						</p>
					{/if}
				</details>
			</div>
		{/if}

		<div class="bus" class:with-detail={!!selected}>
			<Card cls="flush trace-card">
				<header class="trace-head">
					<div>
						<h2>Trace across the buses</h2>
						<p>
							{rows.length}
							{rows.length === 1 ? 'event' : 'events'} recorded against this case
						</p>
					</div>
				</header>
				<!-- The lane header sits outside the scroller: a sticky header inside it covered the
				     first visible row at the resting scroll position. -->
				<div class="seq-h">
					<span>Time</span><span>customer-bus</span><span>worker-bus</span><span>tool-bus</span>
				</div>
				<div class="seq-wrap">
					<div class="seq">
						{#each rows as r (key(r.ev))}
							<button
								type="button"
								class="seq-row"
								class:on={key(r.ev) === openKey}
								aria-pressed={key(r.ev) === openKey}
								onclick={() => (openKey = openKey === key(r.ev) ? '' : key(r.ev))}
							>
								<span class="t">+{fmt.ms(r.t_ms)}</span>
								{#each ['customer', 'worker', 'tool'] as lane}
									<span class="lane" class:has-ev={r.lane === lane}>
										{#if r.lane === lane}
											<span class="lane-tag">{lane}-bus</span>
											<span
												class="ev"
												style="--c:var(--{lane === 'customer'
													? 'cust'
													: lane === 'worker'
														? 'work'
														: 'tool'})"
											>
												<b>{r.ev.envelope.topic.split('.').slice(-2).join('.')}</b>
												<span class="from">{agentName(r.ev.envelope.from)}</span>
												{#if summary(r.ev)}<span class="mono note">{summary(r.ev)}</span>{/if}
											</span>
										{/if}
									</span>
								{/each}
							</button>
						{:else}
							<p class="muted empty">
								No bus events recorded for this case. The trace is rebuilt from the bus streams, so an entry
								that has aged out of them cannot be shown here.
							</p>
						{/each}
					</div>
				</div>
			</Card>
			{#if selected}
				<EventDetail event={selected} />
			{/if}
		</div>

		{#if !focus}
			<details class="band raw">
				<summary>
					Case hash <span class="hint">the raw Redis fields this case is made of</span>
				</summary>
				<JsonView value={d?.hash ?? {}} />
			</details>
		{/if}
	</div>
{:else}
	<Unavailable reason="Case not found." retry={() => goto(back)} />
{/if}

<style>
	.case-page {
		display: flex;
		flex-direction: column;
		gap: 10px;
		/* The admin shell scrolls `.content`, so a percentage height has nothing to resolve against. dvh minus the
		   shell's own chrome (topbar + the content padding) is the only honest measure; min-height keeps it usable
		   when the answer band is open on a short screen, and the page simply scrolls. */
		height: calc(100dvh - 126px);
		min-height: 520px;
	}
	.case-head {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		gap: 12px;
		flex-wrap: wrap;
	}
	.case-head .ident {
		display: grid;
		gap: 4px;
		min-width: 0;
	}
	.case-head .back {
		font-size: 12.5px;
		color: var(--muted);
		text-decoration: none;
	}
	.case-head .back:hover {
		color: var(--fg);
	}
	.case-head h1 {
		margin: 0;
		font: 600 20px/1.2 var(--mono);
		overflow-wrap: anywhere;
	}
	.tags {
		display: flex;
		gap: 8px;
		flex-wrap: wrap;
		align-items: center;
	}
	.head-actions {
		display: flex;
		gap: 8px;
		align-items: center;
	}
	.bands {
		display: grid;
		gap: 8px;
	}
	.band {
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 8px;
		padding: 0 14px;
	}
	.band > summary {
		cursor: pointer;
		padding: 10px 0;
		font: 600 12px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
		display: flex;
		gap: 10px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.band > summary .hint {
		font: 400 11.5px/1.3 var(--sans);
		letter-spacing: 0;
		text-transform: none;
		color: var(--faint);
	}
	.band[open] > summary {
		color: var(--fg);
	}
	.band :global(.kvs) {
		padding-bottom: 12px;
	}
	.answer {
		margin: 0 0 14px;
		font-size: 14px;
		line-height: 1.55;
		white-space: pre-wrap;
		overflow-wrap: anywhere;
		max-width: 88ch;
	}
	.band.raw :global(pre.code) {
		max-height: 320px;
		margin-bottom: 12px;
	}
	.bus {
		flex: 1;
		min-height: 0;
		display: grid;
	}
	.bus.with-detail {
		grid-template-columns: minmax(0, 1fr) minmax(280px, 400px);
		gap: 10px;
	}
	/* `cls` is applied to Card's own root, which a scoped selector cannot reach. */
	:global(.trace-card) {
		min-height: 0;
		display: flex;
		flex-direction: column;
		overflow: hidden;
	}
	/* Card renders its children inside `.card-body`; the column must start there, or `.seq-wrap` never gets a
	   bounded height and the card's overflow:hidden clips every event past the fold. */
	:global(.trace-card) :global(.card-body) {
		display: flex;
		flex-direction: column;
		min-height: 0;
	}
	.trace-head {
		display: flex;
		justify-content: space-between;
		align-items: flex-start;
		padding: 12px 16px 8px;
		gap: 12px;
	}
	.trace-head h2 {
		margin: 0;
		font: 600 15px/1.3 var(--cond);
	}
	.trace-head p {
		margin: 2px 0 0;
		color: var(--muted);
		font-size: 12.5px;
	}
	.seq-wrap {
		flex: 1;
		min-height: 0;
		overflow: auto;
		padding: 0 8px 10px;
	}
	.seq {
		font-size: 12px;
	}
	.seq-h,
	.seq-row {
		display: grid;
		grid-template-columns: 64px repeat(3, minmax(0, 1fr));
		gap: 8px;
		text-align: left;
	}
	/* Outside the scroller, so it never covers a row; 16px matches the scroller's 8px padding
	   plus the row's own 8px, keeping the header columns above the row columns. */
	.seq-h {
		background: var(--panel);
		padding: 6px 16px;
		border-bottom: 1px solid var(--line);
		font: 600 11px var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.seq-row {
		width: 100%;
		border: 0;
		background: none;
		color: inherit;
		font: inherit;
		padding: 3px 8px;
		align-items: start;
		border-radius: 5px;
		cursor: pointer;
	}
	.seq-row:hover {
		background: var(--sunk);
	}
	.seq-row.on {
		background: color-mix(in srgb, var(--accent) 10%, transparent);
	}
	.seq-row .t {
		color: var(--faint);
		padding-top: 5px;
		font-size: 11px;
		font-variant-numeric: tabular-nums;
	}
	.seq-row .lane {
		border-left: 1px dashed var(--line);
		margin-left: 10px;
		min-width: 0;
	}
	.ev {
		display: grid;
		gap: 1px;
		padding: 5px 8px;
		border-radius: 5px;
		border-left: 3px solid var(--c);
		background: color-mix(in srgb, var(--c) 8%, var(--panel));
		min-width: 0;
		overflow-wrap: anywhere;
	}
	.ev b {
		color: var(--c);
		font-weight: 500;
		font-size: 12px;
	}
	.ev .from {
		font-size: 11.5px;
		color: var(--muted);
	}
	.ev .note {
		font-size: 11px;
	}
	/* Only the stacked phone layout needs the lane named per row; the desktop header already says it. */
	.lane-tag {
		display: none;
	}
	.empty {
		padding: 14px 8px;
		max-width: 70ch;
	}
	@media (max-width: 1000px) {
		.bus.with-detail {
			grid-template-columns: minmax(0, 1fr);
			grid-template-rows: minmax(180px, 1fr) minmax(180px, 1fr);
		}
	}
	@media (max-width: 720px) {
		.case-page {
			height: auto;
			min-height: 0;
		}
		.bus {
			min-height: 60dvh;
		}
		/* A four-column lane grid cannot fit a phone; sideways scrolling inside the trace was the same
		   clipped-text problem the drawer had. Stack it: time, then the one lane the event belongs to. */
		.seq-h {
			display: none;
		}
		.seq-row {
			grid-template-columns: 52px minmax(0, 1fr);
			gap: 6px;
			padding: 3px 2px;
		}
		.seq-row .lane {
			margin-left: 0;
			border-left: 0;
		}
		.seq-row .lane:not(.has-ev) {
			display: none;
		}
		/* The header that names the lanes is gone on a phone, so each row says which bus it is on. */
		.lane-tag {
			display: block;
			font: 600 10.5px/1.2 var(--cond);
			letter-spacing: 0.06em;
			text-transform: uppercase;
			color: var(--faint);
			margin-bottom: 2px;
		}
	}
</style>
