<script lang="ts">
	import { tokens } from '$lib/console/bus';
	let { value }: { value: unknown } = $props();
	const toks = $derived(tokens(value));
</script>

<pre class="jpre">{#each toks as t, i (i)}{#if t.c}<span class={t.c}>{t.t}</span
			>{:else}{t.t}{/if}{/each}</pre>
