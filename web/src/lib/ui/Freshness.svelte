<script lang="ts">
	let {
		resource,
	}: { resource: { refreshing: boolean; updatedAt: Date | null; refresh: () => Promise<void> } } = $props();
</script>

<div class="row">
	<button class="btn sm" disabled={resource.refreshing} onclick={() => resource.refresh()}
		>{resource.refreshing ? 'Refreshing…' : 'Refresh'}</button
	><span class="muted" role="status"
		>{resource.updatedAt
			? `Last updated ${resource.updatedAt.toLocaleTimeString()}`
			: 'Not refreshed yet'}</span
	>
</div>
