<script lang="ts">
	import { app } from '$lib/app.svelte';
	import { jobPath } from '$lib/scope';
	import Card from './Card.svelte';
	import Status from './Status.svelte';
</script>

{#if app.jobs.length === 0}
	<div class="empty">This tenant has no jobs yet. Jobs arrive from the configurator.</div>
{:else}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl" data-testid="job-list">
				<thead><tr><th>Job</th><th>Key</th><th>Agents</th></tr></thead>
				<tbody>
					{#each app.jobs as j (j.key)}
						<tr>
							<td
								><a href={jobPath(j.key)} aria-current={j.key === app.job ? 'true' : undefined}>{j.label}</a
								></td
							>
							<td class="mono">{j.key}</td>
							<td
								><Status
									word={j.on_bus ? 'ok' : 'queued'}
									label={j.on_bus ? 'Serving' : 'Not on the bus'}
								/></td
							>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}
