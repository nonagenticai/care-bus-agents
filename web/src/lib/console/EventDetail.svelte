<script lang="ts">
	// One bus entry, in full: what it means, its payload, and the raw XADD fields.
	//
	// The console's Inspector (routes/console/Inspector.svelte) does this and more (agent work lists, a live
	// lane sidebar) against `.console`-scoped CSS. This is the same three views without the console layout, so
	// the case page can show an event beside the trace without dragging the console stylesheet along. The data
	// half is shared, not copied: `summary`/`plain`/`links`/`clock` all come from $lib/console/bus.
	import type { BusEvent } from '$lib/api/types';
	import { clock, links, plain, summary } from '$lib/console/bus';
	import { laneOf } from '$lib/console/console.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';

	let { event }: { event: BusEvent | null } = $props();

	type View = 'summary' | 'payload' | 'raw';
	const VIEWS: { id: View; label: string }[] = [
		{ id: 'summary', label: 'Summary' },
		{ id: 'payload', label: 'Payload' },
		{ id: 'raw', label: 'Raw' },
	];
	let view = $state<View>('summary');
	const found = $derived(event ? links(event) : []);
</script>

<aside class="evdetail" aria-label="Event detail">
	{#if !event}
		<p class="muted">Select a row to see its payload and envelope.</p>
	{:else}
		<header>
			<span class="label">{laneOf(event.stream)}-bus · {event.stream}</span>
			<h3>{event.envelope.topic}</h3>
			<p class="mono id">{event.entry_id}</p>
		</header>
		<div class="vtabs" role="tablist" aria-label="Event detail view">
			{#each VIEWS as v (v.id)}
				<button type="button" role="tab" aria-selected={view === v.id} onclick={() => (view = v.id)}>
					{v.label}
				</button>
			{/each}
		</div>
		<div class="body">
			{#if view === 'summary'}
				<p class="plain">{plain(event) || 'No plain-language summary for this topic.'}</p>
				{#if summary(event)}<p>{summary(event)}</p>{/if}
				<p class="muted">
					{event.envelope.from} → {event.consumer_group ?? 'no consumer group'} · {clock(
						event.envelope.published_at_ms,
					)}
					{#if event.acked === true}· acked{:else if event.acked === false}· not acked{/if}
				</p>
				{#if found.length}
					<div class="links">
						{#each found as l (l.kind + l.label)}
							<a href={l.href}>{l.kind}: {l.label}</a>
						{/each}
					</div>
				{:else}
					<p class="muted">This event names no case, tool or procedure.</p>
				{/if}
			{:else if view === 'payload'}
				<JsonView value={event.envelope.payload} />
			{:else}
				<h4>XADD {event.stream}</h4>
				<JsonView
					value={{
						stream: event.stream,
						entry_id: event.entry_id,
						consumer_group: event.consumer_group,
						acked: event.acked,
					}}
				/>
				<h4>field “envelope”</h4>
				<JsonView value={event.envelope} />
			{/if}
		</div>
	{/if}
</aside>

<style>
	.evdetail {
		display: flex;
		flex-direction: column;
		min-height: 0;
		min-width: 0;
		background: var(--panel);
		border: 1px solid var(--line);
		border-radius: 8px;
		overflow: hidden;
	}
	.evdetail > .muted {
		padding: 14px 16px;
	}
	header {
		padding: 12px 14px 10px;
		border-bottom: 1px solid var(--line);
		min-width: 0;
	}
	header h3 {
		margin: 3px 0 0;
		font: 600 13px/1.35 var(--cond);
		overflow-wrap: anywhere;
	}
	header .id {
		margin: 3px 0 0;
		color: var(--faint);
		font-size: 11.5px;
		overflow-wrap: anywhere;
	}
	.vtabs {
		display: flex;
		gap: 2px;
		padding: 8px 10px 0;
		border-bottom: 1px solid var(--line);
	}
	.vtabs button {
		border: 0;
		background: none;
		padding: 6px 9px;
		border-radius: 5px 5px 0 0;
		color: var(--muted);
		font: 500 12px var(--sans);
		cursor: pointer;
	}
	.vtabs button[aria-selected='true'] {
		color: var(--fg);
		background: var(--sunk);
	}
	.body {
		padding: 12px 14px 14px;
		overflow: auto;
		min-height: 0;
		display: grid;
		gap: 8px;
		align-content: start;
		font-size: 13px;
	}
	.body h4 {
		margin: 6px 0 0;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	.body p {
		margin: 0;
		overflow-wrap: anywhere;
	}
	.body .plain {
		color: var(--fg);
	}
	.links {
		display: flex;
		flex-wrap: wrap;
		gap: 10px;
		font-size: 12.5px;
	}
</style>
