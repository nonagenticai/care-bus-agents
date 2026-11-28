<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { childrenSummary, RUN_LABELS, replaceSummary, resyncToast } from '$lib/sources';
	import type { Paged, Source, SourceRun } from '$lib/api/types';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import FilterInput from '$lib/ui/FilterInput.svelte';
	import { debounce } from '$lib/debounce';

	let { source, canWrite, onchange }: { source: Source; canWrite: boolean; onchange: () => void } = $props();
	/** Older-runs cursor: `''` is the newest page. */
	let before = $state('');
	// The runs search is the server's: `q` goes to the API, not over the loaded page.
	let filter = $state('');
	let q = $state('');
	const pushQ = debounce(() => (q = filter.trim()), 300);
	$effect(() => {
		// Clearing the box applies at once; typing waits out the debounce.
		if (filter.trim() === '') q = '';
		else pushQ();
	});
	const runs = new Resource<Paged<SourceRun>>(() =>
		api().sourceRuns(source.id, { q: q || undefined, limit: 20, before: before || undefined }),
	);
	$effect(() => {
		void source.id;
		void q;
		// A new source or a new search starts at the newest runs: the cursor belongs to the old list.
		before = '';
		runs.refresh();
	});
	function older(next: string) {
		before = next;
		runs.refresh();
	}
	function newer() {
		before = '';
		runs.refresh();
	}
	const next = $derived(runs.value.state === 'ok' ? runs.value.data.next : null);
	let files = $state<File[]>([]);
	let busy = $state(false);
	const running = $derived(
		runs.value.state === 'ok' && runs.value.data.items.some((r) => r.status === 'running'),
	);
	const blocked = $derived(
		!canWrite
			? 'Needs the tenant_admin role'
			: running
				? 'A run is still in progress'
				: !source.enabled
					? 'Enable the source first'
					: source.type === 'file' && files.length === 0
						? 'Choose the files to ingest'
						: '',
	);
	const when = (t: string | null) => (t ? new Date(t).toLocaleString('en-GB') : '—');

	async function resync() {
		busy = true;
		const started: { run: SourceRun | null } = { run: null };
		const ok = await guard(
			async () => {
				started.run = await api().resync(source.id, source.type === 'file' ? files : undefined);
			},
			() => (started.run ? resyncToast(started.run) : 'Re-sync started.'),
		);
		busy = false;
		if (ok) {
			files = [];
			runs.refresh();
			onchange();
		}
	}
</script>

<section class="runs">
	<div class="row">
		<h3>Runs</h3>
		{#if source.type === 'file'}
			<input
				type="file"
				multiple
				aria-label="Files to ingest"
				disabled={!canWrite}
				onchange={(e) => (files = [...(e.currentTarget.files ?? [])])}
			/>
		{/if}
		<button
			class="btn primary"
			type="button"
			disabled={busy || blocked !== ''}
			title={blocked}
			onclick={resync}
		>
			Re-sync
		</button>
		<button class="btn" type="button" onclick={() => runs.refresh()}>Refresh</button>
	</div>
	<FilterInput
		bind:value={filter}
		label="Search runs"
		placeholder="Search status, who started it, or error"
	/>
	{#if runs.value.state === 'unavailable'}
		<Unavailable reason={runs.value.reason} retry={() => runs.refresh()} />
	{:else if runs.value.state === 'empty'}
		<div class="empty">{q ? 'No runs match this search.' : 'Not synced yet.'}</div>
	{:else if runs.value.state === 'ok'}
		<div class="tbl-wrap">
			<table class="tbl dense stack-phone">
				<thead><tr><th>Started</th><th>Status</th><th class="r">Documents</th><th>Details</th></tr></thead>
				<tbody>
					{#each runs.value.data.items as r}
						<tr>
							<td data-label="Started">{when(r.started_at)}<span class="cell-sub">{r.started_by}</span></td>
							<td data-label="Status">
								<Status word={r.status} label={RUN_LABELS[r.status]} />
								{#if r.children}<span class="cell-sub">{childrenSummary(r.children)}</span>{/if}
								{#if r.stale}<span class="cell-sub">
										strata_ingest is unreachable; this status may be behind
									</span>{/if}
							</td>
							<td class="r" data-label="Documents">
								{r.status === 'running' ? '—' : fmt.n(r.document_ids.length)}
							</td>
							<td data-label="Details">
								{#if r.error}<span class="cell-sub">{r.error}</span>{/if}
								{#if r.replace_report}<span class="cell-sub">{replaceSummary(r.replace_report)}</span>{/if}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		<div class="row">
			<button class="btn" type="button" disabled={!before || runs.refreshing} onclick={newer}>
				Newer results
			</button>
			<span class="muted" role="status">
				{runs.value.data.items.length} runs on this page{next ? ' · more available' : ' · end'}
			</span>
			<button
				class="btn"
				type="button"
				disabled={!next || runs.refreshing}
				onclick={() => next && older(next)}
			>
				Older results
			</button>
		</div>
	{:else}
		<p class="muted">Loading…</p>
	{/if}
</section>

<style>
	.runs {
		margin-top: 16px;
	}
</style>
