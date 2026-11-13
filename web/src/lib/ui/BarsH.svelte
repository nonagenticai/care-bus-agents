<script lang="ts">
	import type { Snippet } from 'svelte';
	import { fmt } from '$lib/format';
	type Item = { label: string; parts: { v: number; cls: string; label: string }[]; right?: string };
	let {
		items,
		onClick,
		ariaLabel = '',
	}: { items: Item[]; onClick?: (i: number) => void; ariaLabel?: string } = $props();
	const max = $derived(Math.max(1, ...items.map((it) => it.parts.reduce((a, p) => a + p.v, 0))));
</script>

{#snippet row(it: Item)}
	<span class="bar-label">{it.label}</span>
	<span class="bar-track"
		>{#each it.parts as p}<i class={p.cls} style="width:{(p.v / max) * 100}%" title="{p.label}: {fmt.n(p.v)}"
			></i>{/each}</span
	>
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
</div>

<details>
	<summary>{ariaLabel ? `View data: ${ariaLabel}` : 'View bar data'}</summary>
	<table class="tbl dense">
		<thead><tr><th>Category</th><th>Series</th><th>Value</th></tr></thead><tbody
			>{#each items as it}{#each it.parts as p}<tr><th>{it.label}</th><td>{p.label}</td><td>{p.v}</td></tr
					>{/each}{/each}</tbody
		>
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
