<script lang="ts">
	let {
		items,
		active = $bindable(),
		label,
	}: { items: [string, string][]; active: string; label?: string } = $props();
</script>

<div class="seg" role="group" aria-label={label}>
	{#each items as [id, label]}
		<button
			type="button"
			class="tab"
			class:on={id === active}
			aria-pressed={id === active}
			onclick={() => (active = id)}>{label}</button
		>
	{/each}
</div>
