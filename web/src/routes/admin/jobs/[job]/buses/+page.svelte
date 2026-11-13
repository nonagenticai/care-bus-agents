<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Latest, Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { deadLetter, groupHealth, streamHealth } from '../cases/lanes';
	import {
		agentName,
		areaLabel,
		claimBlocked,
		entryLine,
		entryTime,
		groupChipText,
		publisherName,
		replayBlocked,
		streamLabel,
	} from './labels';
	import type { Entry, Paged, StreamInfo } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	let tab = $state('streams');
	const streams = new Resource<Paged<StreamInfo>>(() => api().streams());
	const dead = new Resource<Paged<Entry>>(() => api().deadLetters());
	streams.refresh();
	dead.refresh();
	const both = {
		get refreshing() {
			return streams.refreshing || dead.refreshing;
		},
		get updatedAt() {
			const t = [streams.updatedAt, dead.updatedAt].filter((d): d is Date => !!d);
			return t.length ? new Date(Math.min(...t.map((d) => d.getTime()))) : null;
		},
		refresh: async () => {
			await Promise.all([streams.refresh(), dead.refresh()]);
		},
	};
	let open = $state(false);
	let sel = $state<StreamInfo | null>(null);
	let entries = $state<Entry[]>([]);
	let entriesError = $state('');
	let entriesLoading = $state(false);
	const admin = $derived(session.can('tenant_admin'));
	const busVariant = {
		customer: 'cust',
		worker: 'work',
		tool: 'tool',
		dead: 'dead',
		other: 'muted',
	} as const;
	const busName = (b: StreamInfo['bus']) =>
		b === 'dead' ? 'dead-letter' : b === 'other' ? 'other' : `${b}-bus`;

	const latestEntries = new Latest();
	async function openStream(s: StreamInfo) {
		const ticket = latestEntries.next();
		sel = s;
		open = true;
		entries = [];
		entriesError = '';
		entriesLoading = true;
		try {
			const items = (await api().streamEntries(s.key, 20)).items;
			if (latestEntries.stale(ticket)) return;
			entries = items;
		} catch (e) {
			if (latestEntries.stale(ticket)) return;
			entriesError = (e as Error).message;
		} finally {
			if (!latestEntries.stale(ticket)) entriesLoading = false;
		}
	}

	async function claim(key: string, group: string) {
		let n = 0;
		const ok = await guard(
			async () => {
				n = (await api().claim(key, group)).claimed;
			},
			() =>
				n === 0
					? `Nothing to take over for ${agentName(group)}: no entry has been idle 30 s or more.`
					: `Took over ${n} stuck entr${n === 1 ? 'y' : 'ies'} for ${agentName(group)}.`,
		);
		if (!ok) return;
		await streams.refresh();
		if (streams.value.state === 'ok') sel = streams.value.data.items.find((s) => s.key === key) ?? sel;
	}
</script>

<header class="page-head">
	<div>
		<h1>Buses & topics</h1>
		<p>
			Messages move between agents on Redis Streams, one set of streams per department. Each agent reads a
			stream through its consumer group and acknowledges a message once it has handled it.
		</p>
	</div>
	<div class="actions">
		<Freshness resource={both} />
	</div>
</header>
{#if !admin}
	<p class="note role-note">
		You can look but not change: replay, discard and claim need the tenant_admin role.
	</p>
{/if}
<Tabs
	items={[
		['streams', 'Streams'],
		['dead', 'Dead letters', dead.value.state === 'ok' ? dead.value.data.items.length : undefined],
	]}
	bind:active={tab}
>
	{#if tab === 'streams'}
		{#if streams.value.state === 'loading'}
			<p class="muted">Loading…</p>
		{:else if streams.value.state === 'unavailable'}
			<Unavailable reason={streams.value.reason} retry={() => streams.refresh()} />
		{:else if streams.value.state === 'empty'}
			<div class="empty">No streams yet. They appear after the first message is published.</div>
		{:else}
			{@const items = streams.value.data.items}
			{@const groups = items.flatMap((s) => s.groups)}
			<div class="kpis">
				<Kpi label="Streams" value={fmt.n(items.length)} />
				<Kpi label="Consumer groups" value={fmt.n(groups.length)} />
				<Kpi
					label="Pending entries"
					value={fmt.n(groups.reduce((a, g) => a + g.pending, 0))}
					note="delivered, not yet acknowledged"
				/>
				<Kpi
					label="Groups behind"
					value={fmt.n(groups.filter((g) => groupHealth(g) !== 'ok').length)}
					note="lag above 5 or more than 3 pending"
				/>
			</div>
			<Card cls="flush">
				<div class="tbl-wrap">
					<table class="tbl clickable">
						<thead>
							<tr
								><th>Stream</th><th>Bus</th><th class="r">Length</th><th>Consumer groups</th><th>Health</th
								></tr
							>
						</thead>
						<tbody>
							{#each items as s}
								{@const h = streamHealth(s)}
								<tr onclick={() => openStream(s)}>
									<td
										><button
											type="button"
											class="table-action cell-title"
											onclick={(e) => {
												e.stopPropagation();
												openStream(s);
											}}>{streamLabel(s.key)}</button
										><span class="cell-sub mono">{s.key}</span></td
									>
									<td><Chip text={busName(s.bus)} variant={busVariant[s.bus]} /></td>
									<td class="r">{fmt.n(s.length)}</td>
									<td>
										{#if s.groups.length === 0}
											<span class="muted">none</span>
										{:else}
											<div class="chips">
												{#each s.groups as g}
													<Chip
														text={groupChipText(g)}
														title={g.name}
														variant={groupHealth(g) === 'ok' ? 'muted' : groupHealth(g)}
													/>
												{/each}
											</div>
										{/if}
									</td>
									<td><Status word={h.word} label={h.label} /></td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			</Card>
		{/if}
	{:else if dead.value.state === 'loading'}
		<p class="muted">Loading…</p>
	{:else if dead.value.state === 'unavailable'}
		<Unavailable reason={dead.value.reason} retry={() => dead.refresh()} />
	{:else if dead.value.state === 'empty'}
		<div class="empty">No dead letters. A message lands here when its consumer gives up on it.</div>
	{:else}
		<Card
			cls="flush"
			title="Messages a consumer gave up on"
			sub="Replay puts the original message back on its stream. Discard deletes it from bus:dead."
		>
			<div class="tbl-wrap">
				<table class="tbl">
					<thead>
						<tr><th>Case</th><th>From</th><th>Reason</th><th class="r">Deliveries</th><th></th></tr>
					</thead>
					<tbody>
						{#each dead.value.data.items as d}
							{@const p = deadLetter(d)}
							{@const replayWhy = replayBlocked(admin, !!p)}
							<tr>
								<td
									><span class="cell-title mono">{d.envelope.correlation_id}</span><span class="cell-sub"
										>{entryTime(d.entry_id)} · <span class="mono">{d.entry_id}</span></span
									></td
								>
								<td
									><span class="cell-title"
										>{p ? streamLabel(p.stream) : '—'} · {publisherName(d.envelope.from)}</span
									><span class="cell-sub mono">{p?.stream ?? '—'} · {d.envelope.from}</span></td
								>
								<td>{p?.reason ?? 'not a dead letter'}</td>
								<td class="r">{p?.deliveries ?? '—'}</td>
								<td>
									<div class="actions">
										<button
											class="btn sm"
											type="button"
											disabled={!!replayWhy}
											title={replayWhy || `Put it back on ${p?.stream}`}
											onclick={() =>
												guard(
													() => api().replayDeadLetter(d.entry_id),
													`Replayed ${d.envelope.correlation_id} to ${p ? streamLabel(p.stream) : ''}.`,
												).then((ok) => {
													if (!ok) return;
													dead.refresh();
												})}>Replay</button
										>
										<button
											class="btn sm danger"
											type="button"
											disabled={!admin}
											title={admin ? 'Delete it from bus:dead' : 'Needs the tenant_admin role'}
											onclick={() =>
												guard(
													() => api().discardDeadLetter(d.entry_id),
													`Discarded the dead letter for ${d.envelope.correlation_id}.`,
												).then((ok) => {
													if (!ok) return;
													dead.refresh();
												})}>Discard</button
										>
									</div>
									{#if admin && replayWhy}<span class="cell-sub">{replayWhy}</span>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/if}
</Tabs>
<Drawer bind:open title={sel ? streamLabel(sel.key) : ''} wide>
	{#if sel}
		{@const stream = sel}
		<KeyValues
			pairs={[
				['Key', stream.key],
				['Bus', busName(stream.bus)],
				['Area', stream.area ? areaLabel(stream.area) : 'all areas'],
				['Length', fmt.n(stream.length)],
				[
					'Last entry',
					stream.last_entry_id ? `${entryTime(stream.last_entry_id)} (${stream.last_entry_id})` : '—',
				],
			]}
		/>
		<h3>Consumer groups</h3>
		{#if stream.groups.length === 0}
			<p class="muted">No consumer group reads this stream. Entries stay until you replay or discard them.</p>
		{:else}
			<p class="muted">
				Pending means delivered but not yet acknowledged. Claim stuck hands entries idle for 30 s or more to
				another consumer.
			</p>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead>
						<tr
							><th>Group</th><th class="r">Consumers</th><th class="r">Pending</th><th class="r">Lag</th><th
								>Last delivered</th
							><th></th></tr
						>
					</thead>
					<tbody>
						{#each stream.groups as g}
							{@const claimWhy = claimBlocked(admin, g.pending)}
							<tr>
								<td
									><span class="cell-title">{agentName(g.name)}</span><span class="cell-sub mono"
										>{g.name}</span
									></td
								>
								<td class="r">{g.consumers}</td>
								<td class="r">{g.pending}</td>
								<td class="r">{g.lag ?? '—'}</td>
								<td
									>{entryTime(g.last_delivered_id) || '—'}<span class="cell-sub mono"
										>{g.last_delivered_id}</span
									></td
								>
								<td>
									<button
										class="btn sm"
										type="button"
										disabled={!!claimWhy}
										title={claimWhy || 'Hand entries idle 30 s or more to another consumer'}
										onclick={() => claim(stream.key, g.name)}>Claim stuck</button
									>
									{#if admin && claimWhy}<span class="cell-sub">{claimWhy}</span>{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
		<h3>Recent entries</h3>
		{#if entriesLoading}
			<p class="muted">Loading…</p>
		{:else if entriesError}
			<Unavailable reason={entriesError} retry={() => openStream(stream)} />
		{:else if entries.length === 0}
			<div class="empty">No entries on this stream.</div>
		{:else}
			<ul class="list-plain entries">
				{#each entries as e (e.entry_id)}
					{@const l = entryLine(e)}
					<li>
						<details>
							<summary>
								<span class="mono muted">{l.time}</span>
								<Chip text={l.event} variant={busVariant[stream.bus]} />
								<span
									>{l.from}{#if l.to}
										→ {l.to}{/if}</span
								>
								{#if l.detail}<span class="muted">· {l.detail}</span>{/if}
								<span class="cell-sub mono">{e.envelope.correlation_id} · {e.entry_id}</span>
							</summary>
							<JsonView value={e} />
						</details>
					</li>
				{/each}
			</ul>
		{/if}
	{/if}
</Drawer>

<style>
	.role-note {
		margin: 0 0 10px;
	}
	.entries li {
		display: block;
	}
	.entries summary {
		cursor: pointer;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 8px;
		align-items: baseline;
		padding: 6px 0;
	}
	.entries summary .cell-sub {
		flex-basis: 100%;
	}
	.entries details[open] summary {
		padding-bottom: 8px;
	}
</style>
