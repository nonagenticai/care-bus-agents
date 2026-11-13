<script lang="ts">
	let { value, cls = 's-accent' }: { value: number; cls?: string } = $props();
</script>

<span class="meter {cls}"><i style="width:{Math.max(0, Math.min(1, value)) * 100}%"></i></span>
