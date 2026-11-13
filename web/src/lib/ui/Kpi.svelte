<script lang="ts">
	// ui.js kpi, plus the unavailable state (shows "—" and the reason, never 0).
	let {
		label,
		value,
		delta = null,
		good = 'up',
		note = '',
		unavailable = '',
	}: {
		label: string;
		value: string | null;
		delta?: number | null;
		good?: 'up' | 'down';
		note?: string;
		unavailable?: string;
	} = $props();
	const isGood = $derived(delta == null ? true : (good === 'up') === delta >= 0);
</script>

<div class="kpi">
	<span class="label">{label}</span>
	<span class="kv-val">{unavailable ? '—' : (value ?? '—')}</span>
	<span class="kpi-foot">
		{#if unavailable}
			<span class="vs">Unavailable: {unavailable}</span>
		{:else if delta != null}
			<span class="delta" class:good={isGood} class:bad={!isGood}
				>{delta >= 0 ? '▲' : '▼'} {Math.abs(delta * 100).toFixed(1)}%</span
			>
			<span class="vs">vs prev. period</span>
		{/if}
		{#if note}<span class="vs">{note}</span>{/if}
	</span>
</div>
