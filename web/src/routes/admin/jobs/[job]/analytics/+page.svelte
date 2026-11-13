<script lang="ts">
	import { api } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { analyticsKpis, kpiDelta, toolBudgetMs, toSeries, type KpiView } from './series';
	import type { Analytics } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import LineChart from '$lib/ui/LineChart.svelte';
	import BarsH from '$lib/ui/BarsH.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import Kpi from '$lib/ui/Kpi.svelte';

	let range = $state('28');
	const data = new Resource<Analytics>(
		() => api().analytics(range === '7' ? 7 : 28),
		(a) => a.days.every((d) => d.completed + d.not_eligible + d.failed + d.open === 0),
	);
	const show = (k: KpiView) =>
		k.value == null ? null : k.unit === 'pct' ? fmt.pct(k.value) : fmt.n(k.value);
	const OUTCOMES = [
		{ cls: 's-ok', label: 'Resolved by agents' },
		{ cls: 's-warn', label: 'Not eligible' },
		{ cls: 's-err', label: 'Handed to a person' },
	];
	$effect(() => {
		void range;
		data.refresh();
	});
</script>

<header class="page-head">
	<div>
		<h1>Analytics</h1>
		<p>How cases end, which request types need people, and where the time goes.</p>
	</div>
	<div class="actions">
		<Freshness resource={data} />
		<Seg
			items={[
				['7', '7 days'],
				['28', '28 days'],
			]}
			bind:active={range}
		/>
	</div>
</header>

{#if data.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if data.value.state === 'unavailable'}
	<Unavailable reason={data.value.reason} retry={() => data.refresh()} />
{:else if data.value.state === 'empty'}
	<div class="empty">
		No cases in this period yet. Run a conversation in the <a href="/console">live console</a> to create one.
	</div>
{:else}
	{@const a = data.value.data}
	<div class="kpis">
		{#each analyticsKpis(a) as k}
			<Kpi
				label={k.label}
				value={show(k)}
				delta={kpiDelta(k)}
				good={k.good ?? 'up'}
				unavailable={k.unavailable}
				note={k.label === 'Retries' ? 'work items sent again' : ''}
			/>
		{/each}
	</div>
	<Card title="Cases per day by outcome" sub="Source: case_history, grouped by start day">
		<LineChart
			labels={a.days.map((d) => d.day.slice(5))}
			series={toSeries(a)}
			ariaLabel="Cases per day by outcome"
		/>
	</Card>
	<Card title="Request types by outcome" sub="Finished cases in this period">
		{#if a.by_type.length === 0}
			<div class="empty">No finished cases in this period yet.</div>
		{:else}
			<BarsH
				items={a.by_type.map((t) => ({
					label: t.request_type,
					parts: [
						{ v: t.completed, cls: 's-ok', label: 'Resolved by agents' },
						{ v: t.not_eligible, cls: 's-warn', label: 'Not eligible' },
						{ v: t.failed, cls: 's-err', label: 'Handed to a person' },
					],
				}))}
			/>
			<div class="legend">
				{#each OUTCOMES as o}<span><i class="sw {o.cls}"></i>{o.label}</span>{/each}
			</div>
		{/if}
	</Card>
	{#if a.by_type.length}
		<Card title="Per request type" sub="Auto-resolved means the case finished without a person" cls="flush">
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead>
						<tr
							><th>Request type</th><th class="r">Cases</th><th class="r">Auto-resolved</th><th class="r"
								>Not eligible</th
							><th class="r">Handed to a person</th><th class="r">Retries</th><th class="r">Median time</th
							></tr
						>
					</thead>
					<tbody>
						{#each a.by_type as t}
							<tr>
								<td class="mono">{t.request_type}</td>
								<td class="r">{fmt.n(t.cases)}</td>
								<td class="r">{fmt.pct(t.cases ? t.completed / t.cases : null)}</td>
								<td class="r">{fmt.n(t.not_eligible)}</td>
								<td class="r">{fmt.n(t.failed)}</td>
								<td class="r">{fmt.n(t.retries)}</td>
								<td class="r">{fmt.ms(t.p50_ms)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		</Card>
	{/if}
	<Card
		title="Tool latency"
		sub="Typical (p50) and slow (p95) response time per tool. Each call times out after 3 s by default; FAQ tools get 20 s because a model writes the answer."
		cls="flush"
	>
		<div class="tbl-wrap">
			<table class="tbl dense">
				<thead>
					<tr><th>Tool</th><th class="r">p50</th><th class="r">p95</th><th>p95 against the timeout</th></tr>
				</thead>
				<tbody>
					{#if a.latency.length === 0}
						<tr><td colspan="4" class="muted">No tool calls recorded in this period.</td></tr>
					{/if}
					{#each a.latency as l}
						{@const budget = toolBudgetMs(l.tool)}
						<tr>
							<td class="mono">{l.tool}</td>
							<td class="r">{fmt.ms(l.p50_ms)}</td>
							<td class="r">{fmt.ms(l.p95_ms)}</td>
							<td>
								{#if l.p95_ms != null}
									<span class="budget">
										<Meter
											value={l.p95_ms / budget}
											cls={l.p95_ms > budget ? 's-err' : l.p95_ms > budget * 0.8 ? 's-warn' : 's-ok'}
										/>
										<span class="mono muted">{fmt.ms(l.p95_ms)} / {fmt.ms(budget)}</span>
									</span>
								{:else}
									<span class="muted">No latency recorded for these calls</span>
								{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

<style>
	.budget {
		display: inline-flex;
		gap: 8px;
		align-items: center;
	}
</style>
