<script lang="ts">
	import type { Snippet } from 'svelte';
	let {
		title,
		sub,
		cls = '',
		actions,
		children,
	}: { title?: string; sub?: string; cls?: string; actions?: Snippet; children: Snippet } = $props();
</script>

<section class="card {cls}">
	{#if title}
		<header class="card-head">
			<div>
				<h2>{title}</h2>
				{#if sub}<p>{sub}</p>{/if}
			</div>
			{#if actions}<div class="actions">{@render actions()}</div>{/if}
		</header>
	{/if}
	<div class="card-body">{@render children()}</div>
</section>
