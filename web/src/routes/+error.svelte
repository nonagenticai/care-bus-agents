<script lang="ts">
	import { page } from '$app/state';
	const heading = $derived(page.status === 404 ? 'Page not found' : 'Something went wrong');
</script>

<svelte:head><title>{page.status} · Care Bus</title></svelte:head>

<div class="callout err" role="alert">
	<strong>{page.status} · {heading}</strong>
	<span>{page.error?.message ?? 'Unexpected error.'}</span>
	<span><a href="/admin">Back to the dashboard</a></span>
</div>
