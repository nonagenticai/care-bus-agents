<script lang="ts">
	import { jobPath } from '$lib/scope';
	import Freshness from '$lib/ui/Freshness.svelte';
	import { env } from '$env/dynamic/public';
	import { api, app } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { kpiDelta, overviewKpis, type KpiView } from './analytics/series';
	import type { Overview } from '$lib/api/types';
	import Kpi from '$lib/ui/Kpi.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const overview = new Resource<Overview>(
		() => api().overview(),
		() => false,
	);
	overview.refresh();
	const show = (k: KpiView) =>
		k.value == null
			? null
			: k.unit === 'pct'
				? fmt.pct(k.value)
				: k.unit === 'ms'
					? fmt.ms(k.value)
					: fmt.n(k.value);
	const HEALTH_LABEL = { ok: 'Healthy', down: 'Down', unconfigured: 'Not configured' } as const;
	const SEVERITY_LABEL = { err: 'Critical', warn: 'Warning' } as const;
</script>

<header class="page-head">
	<div>
		<h1>Dashboard</h1>
		<p>{env.PUBLIC_CAREBUS_JOB_LABEL || 'Configured job'} · today.</p>
	</div>
	<div class="actions">
		<a class="btn" href="/console">Open live console</a>
		<Freshness resource={overview} />
	</div>
</header>

{#if overview.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if overview.value.state === 'unavailable'}
	<Unavailable reason={overview.value.reason} retry={() => overview.refresh()} />
{:else if overview.value.state === 'ok'}
	{@const o = overview.value.data}
	<div class="kpis">
		{#each overviewKpis(o) as k}
			<Kpi
				label={k.label}
				value={show(k)}
				delta={kpiDelta(k)}
				good={k.good ?? 'up'}
				unavailable={k.unavailable}
			/>
		{/each}
	</div>
	<div class="grid g-main">
		<Card title="Needs attention" sub="Sorted by impact on customers">
			{#if o.attention.length === 0}<div class="empty">Nothing needs attention right now.</div>{/if}
			<ul class="list-plain">
				{#each o.attention as a}
					<li>
						<a class="attn-link" href={jobPath(app.job, `/${a.page}`)}
							><Status word={a.severity} label={SEVERITY_LABEL[a.severity]} /><span class="cell-title"
								>{a.title}</span
							></a
						>
					</li>
				{/each}
			</ul>
		</Card>
		<Card title="System health">
			<ul class="list-plain">
				{#each o.health as h}
					<li>
						<span
							><span class="cell-title">{h.name}</span>{#if h.detail}<span class="cell-sub mono"
									>{h.detail}</span
								>{/if}</span
						>
						<Status word={h.status} label={HEALTH_LABEL[h.status]} />
					</li>
				{/each}
			</ul>
		</Card>
	</div>
{/if}

<style>
	.attn-link {
		display: flex;
		gap: 10px;
		align-items: flex-start;
		text-decoration: none;
		color: inherit;
		width: 100%;
	}
	.attn-link :global(.st) {
		flex: none;
		min-width: 76px;
		white-space: nowrap;
	}
	.attn-link:hover .cell-title {
		color: var(--accent);
	}
</style>
