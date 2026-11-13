<script lang="ts">
	import { fmt } from '$lib/format';
	type S = { key: string; label: string; cls: string; values: number[] };
	let {
		labels,
		series,
		stacked = true,
		height = 220,
		ariaLabel = '',
	}: { labels: string[]; series: S[]; stacked?: boolean; height?: number; ariaLabel?: string } = $props();
	const W = 720,
		L = 44,
		R = 12,
		T = 12,
		B = 26;
	const geo = $derived.by(() => {
		const n = labels.length,
			iw = W - L - R,
			ih = height - T - B;
		const acc = labels.map(() => 0);
		const layers = series.map((s) => {
			const lo = acc.slice();
			s.values.forEach((v, i) => (acc[i] = (acc[i] ?? 0) + (stacked ? v : 0)));
			return { ...s, lo, hi: stacked ? acc.slice() : s.values.slice() };
		});
		const max = Math.max(1, ...(stacked ? acc : series.flatMap((s) => s.values)));
		const p = Math.pow(10, Math.floor(Math.log10(max)));
		const nice = Math.ceil(max / p) * p;
		const x = (i: number) => L + (i * iw) / Math.max(1, n - 1);
		const y = (v: number) => T + ih - (v / nice) * ih;
		const ticks = [0, 0.25, 0.5, 0.75, 1].map((f) => Math.round(nice * f));
		const step = Math.ceil(n / 7);
		const paths = layers
			.slice()
			.reverse()
			.map((s) => {
				const top = s.hi.map((v, i) => `${x(i).toFixed(1)} ${y(v).toFixed(1)}`);
				const bot = s.lo.map((v, i) => `${x(i).toFixed(1)} ${y(stacked ? v : 0).toFixed(1)}`).reverse();
				return { cls: s.cls, area: `M${top.join('L')}L${bot.join('L')}Z`, line: `M${top.join('L')}` };
			});
		return { x, y, ticks, step, paths, n };
	});
</script>

<div class="chart">
	<svg viewBox="0 0 {W} {height}" role="img" aria-label={ariaLabel}>
		{#each geo.ticks as t}
			<line class="grid" x1={L} x2={W - R} y1={geo.y(t)} y2={geo.y(t)} />
			<text class="ax" x={L - 6} y={geo.y(t) + 4} text-anchor="end">{fmt.k(t)}</text>
		{/each}
		{#each labels as l, i}
			{#if i % geo.step === 0 || i === geo.n - 1}<text
					class="ax"
					x={geo.x(i)}
					y={height - 6}
					text-anchor="middle">{l}</text
				>{/if}
		{/each}
		{#each geo.paths as p}<g class={p.cls}><path class="area" d={p.area} /><path class="line" d={p.line} /></g
			>{/each}
	</svg>
	<div class="legend">
		{#each series as s}<span><i class="sw {s.cls}"></i>{s.label}</span>{/each}
	</div>
</div>

<details>
	<summary>{ariaLabel ? `View data: ${ariaLabel}` : 'View chart data'}</summary>
	<div class="tbl-wrap">
		<table class="tbl dense">
			<caption>{ariaLabel}</caption><thead
				><tr
					><th>Period</th>{#each series as s}<th>{s.label}</th>{/each}</tr
				></thead
			><tbody
				>{#each labels as l, i}<tr
						><th>{l}</th>{#each series as s}<td>{s.values[i] ?? '—'}</td>{/each}</tr
					>{/each}</tbody
			>
		</table>
	</div>
</details>
