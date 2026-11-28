<script lang="ts">
	import type { Snippet } from 'svelte';
	import { fmt } from '$lib/format';
	type Item = { label: string; parts: { v: number; cls: string; label: string }[]; right?: string };
	let {
		items,
		onClick,
		ariaLabel = '',
	}: { items: Item[]; onClick?: (i: number) => void; ariaLabel?: string } = $props();
	const total = (it: Item) => it.parts.reduce((a, p) => a + p.v, 0);
	const max = $derived(Math.max(1, ...items.map(total)));
	/** A round axis end (1/2/5 × 10ⁿ) so the ticks are whole numbers, not the raw largest bar. */
	const axisMax = $derived.by(() => {
		const p = 10 ** Math.floor(Math.log10(max));
		const m = [1, 2, 5, 10].find((x) => max <= x * p) ?? 10;
		return m * p;
	});
	const mid = $derived(axisMax % 2 === 0 ? fmt.n(axisMax / 2) : '');
</script>

{#snippet row(it: Item)}
	<span class="bar-label" title={it.label}>{it.label}</span>
	<span class="bar-track">
		{#each it.parts as p}<i
				class={p.cls}
				style="width:{(p.v / axisMax) * 100}%"
				title="{p.label}: {fmt.n(p.v)}"
			></i>{/each}
	</span>
	<span class="bar-val">{it.right ?? fmt.n(it.parts.reduce((a, p) => a + p.v, 0))}</span>
{/snippet}

<div class="barsh">
	{#each items as it, i}
		{#if onClick}
			<button type="button" class="bar-row bar-btn" onclick={() => onClick(i)}>{@render row(it)}</button>
		{:else}
			<div class="bar-row">{@render row(it)}</div>
		{/if}
	{/each}
	<!-- The scale the bars are drawn against; the table below is the accessible copy. -->
	<div class="barsh-axis" aria-hidden="true">
		<span></span>
		<span class="bar-axis">
			<i>0</i>{#if mid}<i>{mid}</i>{/if}<i>{fmt.n(axisMax)}</i>
		</span>
		<span class="bar-val"></span>
	</div>
</div>

<details>
	<summary>{ariaLabel ? `View data: ${ariaLabel}` : 'View bar data'}</summary>
	<table class="tbl dense">
		<thead><tr><th>Category</th><th>Series</th><th>Value</th></tr></thead><tbody>
			{#each items as it}{#each it.parts as p}<tr>
						<th>{it.label}</th><td>{p.label}</td><td>{p.v}</td>
					</tr>{/each}{/each}
		</tbody>
	</table>
</details>

<style>
	.bar-btn {
		width: 100%;
		background: none;
		border: 0;
		padding: 0;
		color: inherit;
		text-align: inherit;
		cursor: pointer;
	}
</style>
