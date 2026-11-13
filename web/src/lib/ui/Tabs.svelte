<script lang="ts">
	import type { Snippet } from 'svelte';
	let {
		items,
		active = $bindable(),
		children,
		label = 'Views',
	}: { items: [string, string, number?][]; active: string; children?: Snippet; label?: string } = $props();
	const uid = $props.id();
	function key(e: KeyboardEvent, i: number) {
		const target =
			e.key === 'ArrowRight'
				? (i + 1) % items.length
				: e.key === 'ArrowLeft'
					? (i + items.length - 1) % items.length
					: e.key === 'Home'
						? 0
						: e.key === 'End'
							? items.length - 1
							: -1;
		if (target < 0) return;
		e.preventDefault();
		active = items[target]![0];
		document.getElementById(`${uid}-tab-${active}`)?.focus();
	}
</script>

<div class="tabs" role="tablist" aria-label={label}>
	{#each items as [id, label, count], i}
		<button
			type="button"
			role="tab"
			id="{uid}-tab-{id}"
			aria-controls={children ? `${uid}-panel` : undefined}
			tabindex={id === active ? 0 : -1}
			class="tab"
			class:on={id === active}
			aria-selected={id === active}
			onclick={() => (active = id)}
			onkeydown={(e) => key(e, i)}
		>
			{label}{#if count != null}<span class="count">{count}</span>{/if}
		</button>
	{/each}
</div>
{#if children}<div role="tabpanel" id="{uid}-panel" aria-labelledby="{uid}-tab-{active}" tabindex="0">
		{@render children()}
	</div>{/if}
