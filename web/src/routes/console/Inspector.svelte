<script lang="ts">
	import type { BusEvent } from '$lib/api/types';
	import { laneOf } from '$lib/console/console.svelte';
	import { clock, eventOf, links, plain, summary, type AgentCard, type WorkItem } from '$lib/console/bus';
	import { direction } from '$lib/console/console.svelte';
	import JsonPre from './JsonPre.svelte';

	let {
		event,
		agent,
		ack,
	}: { event: BusEvent | null; agent: AgentCard | null; ack: 'acked' | 'pending' | null } = $props();

	// consumer_group and ack come from the server (carebus Streams::consumer_group, the recipient's XACK).
	const entry = $derived.by(() => {
		if (!event) return null;
		return {
			stream: event.stream,
			entry_id: event.entry_id,
			consumer_group: event.consumer_group,
			ack,
			direction: direction(event.envelope.topic) === 'down' ? 'forward' : 'return path',
		};
	});
	type View = 'summary' | 'payload' | 'raw';
	const VIEWS: { id: View; label: string }[] = [
		{ id: 'summary', label: 'Summary' },
		{ id: 'payload', label: 'Payload' },
		{ id: 'raw', label: 'Raw' },
	];
	let view = $state<View>('summary');
	const found = $derived(event ? links(event) : []);
	const outstanding = $derived(agent ? agent.items.filter((w) => w.status === 'run') : []);
	const finished = $derived(agent ? agent.items.filter((w) => w.status !== 'run') : []);
	const STATUS = { run: 'outstanding', ok: 'succeeded', fail: 'failed' } as const;
	const label = (w: WorkItem) =>
		`${eventOf(w.event.envelope.topic) ?? w.event.envelope.topic} · ${w.event.envelope.correlation_id}`;
</script>

<aside class="panel inspector" aria-label="Inspector">
	<header>
		{#if event}
			<span class="label">Stream entry on {laneOf(event.stream)}-bus</span>
			<h3>{event.envelope.topic} · {event.entry_id}</h3>
		{:else if agent}
			<span class="label">Agent state · inferred from bus events and /api/agents</span>
			<h3>{agent.name}</h3>
		{:else}
			<span class="label">Stream entry</span>
			<h3>Pick a message or an agent</h3>
		{/if}
	</header>
	{#if event}
		<div class="vtabs" role="tablist" aria-label="Inspector view">
			{#each VIEWS as v (v.id)}
				<button
					type="button"
					role="tab"
					id="itab-{v.id}"
					aria-selected={view === v.id}
					aria-controls="ipanel"
					onclick={() => (view = v.id)}>{v.label}</button
				>
			{/each}
		</div>
	{/if}
	<div
		class="ibody"
		id="ipanel"
		role={event ? 'tabpanel' : undefined}
		aria-labelledby={event ? `itab-${view}` : undefined}
	>
		{#if event && entry}
			{#if view === 'summary'}
				<p class="plain">{plain(event) || 'No plain-language summary for this topic.'}</p>
				{#if summary(event)}<p>{summary(event)}</p>{/if}
				<p>
					{event.envelope.from} → {event.consumer_group ?? 'no consumer group'} · {entry.direction} · {clock(
						event.envelope.published_at_ms,
					)}{ack ? ` · ${ack}` : ''}
				</p>
				{#if found.length}
					<div class="ilinks">
						{#each found as l (l.kind + l.label)}
							<a href={l.href}>{l.kind}: {l.label}</a>
						{/each}
					</div>
				{:else}
					<p>This event names no case, tool or procedure.</p>
				{/if}
			{:else if view === 'payload'}
				<div>
					<h4>payload</h4>
					<JsonPre value={event.envelope.payload} />
				</div>
			{:else}
				<div>
					<h4>XADD {event.stream}</h4>
					<JsonPre value={entry} />
				</div>
				<div>
					<h4>field "envelope" (Envelope JSON)</h4>
					<JsonPre value={event.envelope} />
				</div>
			{/if}
		{:else if agent}
			<p>
				Status: <b>{agent.status}</b> · {agent.line}
			</p>
			<p>
				Event-derived: working and waiting are inferred from unanswered events seen in this conversation, not
				reported by the agent.
			</p>
			{#if agent.view}
				<div>
					<h4>runtime · /api/agents</h4>
					<JsonPre
						value={{
							group: agent.view.group,
							consumers: agent.view.consumers,
							pending: agent.view.pending,
							lag: agent.view.lag,
							status: agent.view.status,
							streams: agent.view.streams,
							model_routes: agent.view.model_routes,
						}}
					/>
				</div>
			{:else}
				<p>/api/agents has not answered; runtime state is unknown.</p>
			{/if}
			{#each [{ title: 'outstanding-work', list: outstanding }, { title: 'finished-work', list: finished }] as sec (sec.title)}
				<div>
					<h4>{sec.title} ({sec.list.length})</h4>
					{#if sec.list.length}
						<ul class="wl">
							{#each [...sec.list].reverse() as w (w.key)}
								<li>
									<span class="st {w.status}">{STATUS[w.status]}</span>{label(w)}<br />{summary(w.event)}
								</li>
							{/each}
						</ul>
					{:else}
						<p>None in this conversation.</p>
					{/if}
				</div>
			{/each}
		{:else}
			<p>Click any message to see its stream entry and envelope, or an agent to see its work.</p>
		{/if}
	</div>
</aside>
