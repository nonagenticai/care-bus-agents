<script lang="ts">
	let { pairs }: { pairs: [string, string][] } = $props();
</script>

<dl class="kvs">
	{#each pairs as [k, v]}<dt>{k}</dt>
		<dd>{v}</dd>{/each}
</dl>
