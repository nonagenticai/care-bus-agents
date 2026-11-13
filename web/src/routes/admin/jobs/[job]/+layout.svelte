<script lang="ts">
	import { page } from '$app/state';
	import { app, selectJob } from '$lib/app.svelte';
	import JobList from '$lib/ui/JobList.svelte';

	let { children } = $props();
	const job = $derived(page.params.job ?? '');
	const known = $derived(app.jobs.some((j) => j.key === job));
	// The URL names the job: every request of the pages below acts in it. Set before they render, so no page ever
	// reads with the previous job; the root layout remounts the pages when the job changes.
	$effect.pre(() => {
		if (known && app.job !== job) selectJob(job);
	});
</script>

{#if !known}
	<header class="page-head">
		<div>
			<h1>Unknown job</h1>
			<p>This tenant has no job “{job}”. Choose one of its jobs.</p>
		</div>
	</header>
	<JobList />
{:else if app.job === job}
	{@render children()}
{/if}
