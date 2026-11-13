<script lang="ts">
	let { reason, retry }: { reason: string; retry?: () => void } = $props();
</script>

<div class="callout err" role="status">
	<strong>Unavailable</strong>
	<span>{reason}</span>
	{#if retry}<span><button class="btn sm" type="button" onclick={retry}>Try again</button></span>{/if}
</div>
