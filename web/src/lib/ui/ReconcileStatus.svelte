<script lang="ts">
	// The inline result of re-reading a resource after a write whose answer was lost. Never resubmits:
	// "Check again" only re-reads.
	import { reconcileText, type Reconciliation } from '$lib/write-outcome';

	let { result, retry }: { result: Reconciliation | null; retry: () => void } = $props();
	const cls = $derived(
		result?.kind === 'confirmed' ? 'ok' : result?.kind === 'checking' ? 'info' : result ? 'err' : '',
	);
</script>

{#if result}
	<div class="callout reconcile {cls}" role="status">
		<span>{reconcileText(result)}</span>
		{#if result.kind === 'unreachable'}
			<span><button class="btn sm" type="button" onclick={retry}>Check again</button></span>
		{/if}
	</div>
{/if}

<style>
	.reconcile.ok {
		border-color: color-mix(in srgb, var(--ok) 45%, var(--line));
		background: color-mix(in srgb, var(--ok) 7%, var(--panel));
	}
</style>
