<script lang="ts">
	import Freshness from '$lib/ui/Freshness.svelte';
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import type { AgentKind, AgentView, Paged } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import { streamLabel } from '../buses/labels';

	const agents = new Resource<Paged<AgentView>>(() => api().agents());
	agents.refresh();
	let sel = $state<AgentView | null>(null);
	let open = $state(false);
	const GROUPS: [AgentKind, string][] = [
		['front', 'Front'],
		['coordination', 'Coordination'],
		['worker', 'Workers · one per tool'],
		['bridge', 'Bridge to MCP tools'],
	];
	const KIND: Record<AgentKind, string> = {
		front: 'Front (talks to the customer)',
		coordination: 'Coordination (runs the procedure)',
		worker: 'Worker (calls one tool)',
		bridge: 'Bridge (calls MCP tools)',
	};
	const busVariant = (s: string) =>
		s.startsWith('bus:customer')
			? 'cust'
			: s.startsWith('bus:worker')
				? 'work'
				: s.startsWith('bus:tool')
					? 'tool'
					: 'muted';
	// AgentView.status: idle = no consumer in its group yet; warn = lag > 20 or pending > 10 (plan 05 routes::agents)
	const LABEL = { idle: 'No consumers', ok: 'Healthy', warn: 'Behind' } as const;
</script>

<header class="page-head">
	<div>
		<h1>Agents</h1>
		<p>
			Each agent is a small service that reads its streams through a consumer group, does one job and
			publishes the result. Consumers, pending messages and lag are read live from Redis.
		</p>
	</div>
	<div class="actions">
		<Freshness resource={agents} />
	</div>
</header>

{#if agents.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if agents.value.state === 'unavailable'}
	<Unavailable reason={agents.value.reason} retry={() => agents.refresh()} />
{:else if agents.value.state === 'empty'}
	<div class="empty">No agents yet. Start the agent services, then press Refresh.</div>
{:else}
	{#each GROUPS as [kind, label]}
		{@const list = agents.value.data.items.filter((a) => a.kind === kind)}
		{#if list.length}
			<Card title="{label} · {list.length}">
				<div class="grid g3">
					{#each list as a}
						<button
							type="button"
							class="card ag-card"
							onclick={() => {
								sel = a;
								open = true;
							}}
						>
							<div class="split">
								<b>{a.name}</b>
								<Status word={a.status} label={LABEL[a.status]} />
							</div>
							<div class="chips">
								{#each a.model_routes as r}<Chip text={r} variant="accent" />{/each}
								{#if a.model_routes.length === 0}<Chip text="no model" />{/if}
							</div>
							{#if a.streams.length}
								<span class="ag-io"
									><span class="label">Reads</span><span class="chips"
										>{#each a.streams as st}<Chip
												text={streamLabel(st)}
												title={st}
												variant={busVariant(st)}
											/>{/each}</span
									></span
								>
							{/if}
							<span class="muted"
								>{a.consumers} consumer{a.consumers === 1 ? '' : 's'} ·
								<b class:warn-num={a.status === 'warn'}>{a.pending}</b>
								pending · lag <b class:warn-num={a.status === 'warn'}>{a.lag}</b></span
							>
						</button>
					{/each}
				</div>
			</Card>
		{/if}
	{/each}
{/if}

<Drawer bind:open title={sel?.name ?? ''} wide>
	{#if sel}
		<KeyValues
			pairs={[
				['Id', sel.id],
				['Kind', KIND[sel.kind]],
				['Consumer group', sel.group],
				['Tool', sel.tool ?? '—'],
				['Consumers', String(sel.consumers)],
				['Pending', `${sel.pending} (delivered, not yet acknowledged)`],
				['Lag', `${sel.lag} (published, not yet delivered)`],
			]}
		/>
		<h3>Streams it reads</h3>
		{#if sel.streams.length === 0}
			<div class="empty">Its group exists on no stream yet.</div>
		{:else}
			<div class="chips">
				{#each sel.streams as st}<Chip text={streamLabel(st)} title={st} variant={busVariant(st)} />{/each}
			</div>
		{/if}
		<h3>Model routes</h3>
		{#if sel.model_routes.length === 0}
			<p class="muted">None. This agent is deterministic.</p>
		{:else}
			<div class="chips">
				{#each sel.model_routes as r}<Chip text={r} variant="accent" />{/each}
			</div>
			<a class="btn" href="/admin/providers">Edit LLM providers</a>
		{/if}
	{/if}
</Drawer>

<style>
	.ag-card {
		all: unset;
		box-sizing: border-box;
		cursor: pointer;
		display: grid;
		gap: 8px;
		padding: 12px 14px;
		border: 1px solid var(--line);
		border-radius: 8px;
		background: var(--bg);
	}
	.ag-card:hover {
		border-color: var(--muted);
	}
	.ag-io {
		display: grid;
		gap: 4px;
		min-width: 0;
	}
	.warn-num {
		color: var(--warn);
	}
	.ag-card:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
</style>
