<script lang="ts">
	import { goto } from '$app/navigation';
	import { page } from '$app/state';
	import { app } from '$lib/app.svelte';
	import { jobPath, legacyAdminTarget } from '$lib/scope';

	// Pre-W3 admin URLs name no job: send them to the same page of the current job (plan 14). Rendered only once
	// the root layout has finished sign-in, so `app.job` is the tenant's job.
	const target = $derived(legacyAdminTarget(page.url.pathname, page.url.search, page.url.hash, app.job));
	$effect(() => {
		if (target) void goto(target, { replaceState: true });
	});
</script>

{#if !target}
	<header class="page-head">
		<div>
			<h1>Page not found</h1>
			<p>There is no admin page at {page.url.pathname}.</p>
		</div>
	</header>
	<p><a href={jobPath(app.job)}>Go to the dashboard</a></p>
{/if}
