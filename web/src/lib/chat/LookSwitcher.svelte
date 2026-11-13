<script lang="ts">
	import { LOOKS, saveLook, type Look } from './looks';

	/** The chat look; every pick is kept for this viewer (localStorage). */
	let { look = $bindable() }: { look: Look } = $props();
	function pick(id: Look) {
		look = id;
		saveLook(id);
	}
</script>

<div class="seg" role="group" aria-label="Chat look">
	{#each LOOKS as [id, label] (id)}
		<button
			type="button"
			class="tab"
			class:on={id === look}
			aria-pressed={id === look}
			onclick={() => pick(id)}>{label}</button
		>
	{/each}
</div>
