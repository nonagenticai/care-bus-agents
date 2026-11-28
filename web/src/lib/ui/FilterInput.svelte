<script lang="ts">
	// The search box above a list table. Filtering is client-side, so the caller owns the text and decides what
	// the count says; this component only renders the field and its clear button.
	let {
		value = $bindable(),
		label,
		placeholder = 'Search',
		count = '',
	}: {
		value: string;
		label: string;
		placeholder?: string;
		count?: string;
	} = $props();
</script>

<div class="filter">
	<input class="input" type="search" bind:value {placeholder} aria-label={label} />
	{#if value}
		<button type="button" class="btn" onclick={() => (value = '')}>Clear</button>
	{/if}
	{#if count}<span class="muted filter-count" role="status">{count}</span>{/if}
</div>
