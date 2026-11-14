<script lang="ts">
	import { jobPath } from '$lib/scope';
	import { onDestroy, untrack } from 'svelte';
	import { api, app, guard, guardOutcome } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { afterPollError } from '$lib/poll';
	import {
		childrenSummary,
		deleteContinues,
		deleteRefusal,
		euros,
		jobCost,
		jobSourceLabel,
		parentJobOf,
		refusalParent,
	} from '$lib/sources';
	import { toast } from '$lib/ui/toaster.svelte';
	import type { ChildrenSummary, IngestJob, Paged, StoreNode } from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const STATUSES = [
		'queued',
		'running',
		'cancelling',
		'deleting',
		'completed',
		'failed',
		'rejected',
		'cancelled',
	];
	const LIMIT = 100;
	/** While a listed job is still queued, running or cancelling, the list refreshes itself this often. */
	const POLL_MS = 5000;
	let status = $state('');
	let store = $state('');
	const jobs = new Resource<Paged<IngestJob>>(() =>
		api().ingestJobs({ status: status || undefined, store_id: store || undefined, limit: LIMIT }),
	);
	const stores = new Resource<Paged<StoreNode>>(() => api().stores());
	stores.refresh();
	$effect(() => {
		void status;
		void store;
		jobs.refresh();
	});
	const canWrite = $derived(session.can('tenant_admin'));
	const why = $derived(canWrite ? '' : 'Needs the tenant_admin role');
	let open = $state(false);
	let confirmCancel = $state(false);
	let confirmDelete = $state(false);
	/** A 409 to the last delete, shown in the delete confirm (which stays open). */
	let refusal = $state<string | null>(null);
	/** The parent crawl a `child_of_parent_job` refusal named. */
	let refusedParent = $state<string | null>(null);
	/** The job whose delete strata_ingest is finishing in the background (202): polled until it is gone. */
	let bgDeleting = $state<string | null>(null);
	const DELETE_POLL_MS = 2000;
	let cur = $state<IngestJob | null>(null);
	const storeName = (id: string) =>
		(stores.value.state === 'ok' ? stores.value.data.items.find((s) => s.id === id)?.name : undefined) ??
		id.slice(0, 8);
	const active = (j: IngestJob) => ['queued', 'running', 'cancelling'].includes(j.status);
	const polling = $derived(jobs.value.state === 'ok' && jobs.value.data.items.some(active));
	// Each finished refresh schedules the next one while work is in flight; unmount or a quiet list stops it.
	$effect(() => {
		void jobs.value;
		if (!polling) return;
		const t = setTimeout(() => jobs.refresh(), POLL_MS);
		return () => clearTimeout(t);
	});
	// Keep the open drawer on the latest copy of its job after a refresh.
	$effect(() => {
		if (jobs.value.state !== 'ok') return;
		const items = jobs.value.data.items;
		untrack(() => {
			const fresh = cur && items.find((j) => j.id === cur!.id);
			if (fresh && fresh !== cur) cur = fresh;
		});
	});
	// A crawl or sitemap job's page-job summary: in the list when strata_ingest includes it, else read from the job.
	let detail = $state<IngestJob | null>(null);
	// derived, so a fresh copy of the same job (a list refresh, a delete poll) does not read it again
	const detailId = $derived(open ? cur?.id : undefined);
	const listedChildren = $derived(!!cur?.children);
	$effect(() => {
		const id = detailId;
		detail = null;
		if (!id || listedChildren) return;
		let gone = false;
		Promise.resolve()
			.then(() => api().ingestJob(id))
			.then((j) => {
				if (!gone) detail = j;
			})
			.catch(() => {});
		return () => {
			gone = true;
		};
	});
	const pages = $derived<ChildrenSummary | undefined>(cur?.children ?? detail?.children);
	const when = (t: string | null) => (t ? new Date(t).toLocaleString('en-GB') : '—');
	const ended = (j: IngestJob) => ['completed', 'failed', 'rejected', 'cancelled'].includes(j.status);
	/** A crawl or sitemap parent's page jobs that have not ended. */
	const livePages = (c: ChildrenSummary | undefined) =>
		(['queued', 'running', 'cancelling'] as const).reduce((n, st) => n + (c?.by_status[st] ?? 0), 0);
	// strata_ingest PR #11: queued/running cancel; an ended parent still cancels its live page jobs.
	const cancellable = $derived(
		!!cur &&
			(cur.status === 'queued' ||
				cur.status === 'running' ||
				(ended(cur) && cur.status !== 'cancelled' && livePages(pages) > 0)),
	);
	/** A crawl/sitemap page job: strata_ingest deletes it only with its parent (409 `child_of_parent_job`). */
	const parentId = $derived(cur ? parentJobOf(cur) : null);
	// a `deleting` job is one whose DELETE did not finish: DELETE again resumes it (unless it is finishing now)
	const deletable = $derived(
		!!cur &&
			!parentId &&
			bgDeleting !== cur.id &&
			(ended(cur) || cur.status === 'deleting') &&
			livePages(pages) === 0,
	);
	const cancelWhy = $derived(
		!cur
			? ''
			: cur.status === 'cancelling'
				? 'Cancelling: the step in flight finishes first'
				: cur.status === 'deleting'
					? 'Being deleted'
					: cancellable
						? ''
						: 'The job has ended',
	);
	const deleteWhy = $derived(
		!cur || deletable
			? ''
			: bgDeleting === cur.id
				? 'Being deleted in the background'
				: ended(cur)
					? 'Its page jobs are still running: cancel it first'
					: 'Only an ended job can be deleted: cancel it first',
	);
	// strata_ingest's error is `{class, code, message, provider}`: show `code: message`.
	const errText = (e: unknown) => {
		if (e == null) return '';
		if (typeof e !== 'object' || !('message' in e)) return JSON.stringify(e);
		const { code, message } = e as { code?: unknown; message: unknown };
		return typeof code === 'string' ? `${code}: ${String(message)}` : String(message);
	};

	const fresh = {
		get refreshing() {
			return jobs.refreshing;
		},
		get updatedAt() {
			return jobs.updatedAt;
		},
		refresh: async () => {
			if (stores.value.state !== 'ok') void stores.refresh();
			await jobs.refresh();
		},
	};

	async function act(kind: 'retry' | 'cancel') {
		if (!cur) return;
		confirmCancel = false;
		const id = cur.id;
		const box: { v: IngestJob | null } = { v: null };
		const ok = await guard(
			async () => {
				box.v = kind === 'retry' ? await api().retryJob(id) : await api().cancelJob(id);
			},
			() =>
				kind === 'retry'
					? 'Job queued again.'
					: box.v?.status === 'cancelling'
						? 'Cancelling: the step in flight finishes first.'
						: 'Job cancelled.',
		);
		if (ok && box.v) {
			cur = box.v;
			jobs.refresh();
		}
	}
	async function remove() {
		if (!cur) return;
		refusal = null;
		refusedParent = null;
		const id = cur.id;
		let kept = 0;
		let later = false;
		const out = await guardOutcome(
			async () => {
				const r = await api().deleteJob(id);
				if (deleteContinues(r)) later = true;
				else kept = r.document_ids.length;
			},
			() =>
				later
					? 'Deleting… strata_ingest finishes it in the background.'
					: kept
						? `Job deleted. Its ${kept} document${kept === 1 ? '' : 's'} stay${kept === 1 ? 's' : ''} in the knowledge store.`
						: 'Job deleted.',
			'Delete did not finish',
		);
		refusal = out.kind === 'ok' ? null : deleteRefusal(out.error);
		refusedParent = out.kind === 'ok' ? null : refusalParent(out.error);
		// a 409 keeps the confirm open, saying why; anything else closes it (the toast says what happened)
		if (!refusal) confirmDelete = false;
		if (out.kind === 'ok' && later) {
			if (cur?.id === id) cur = { ...cur, status: 'deleting' };
			followDelete(id);
		} else if (out.kind === 'ok') {
			open = false;
			cur = null;
		}
		// a failed delete leaves the job `deleting`: the refreshed list shows it and offers Retry delete
		jobs.refresh();
	}

	// ---- a delete strata_ingest finishes in the background (202): read the job until it is gone (404) ----
	let alive = true;
	let delTimer: ReturnType<typeof setTimeout> | undefined;
	let delFailures = 0;
	onDestroy(() => {
		alive = false;
		clearTimeout(delTimer);
	});
	function followDelete(id: string) {
		clearTimeout(delTimer);
		bgDeleting = id;
		delFailures = 0;
		delTimer = setTimeout(() => pollDelete(id), DELETE_POLL_MS);
	}
	async function pollDelete(id: string) {
		if (!alive || bgDeleting !== id) return;
		try {
			const j = await api().ingestJob(id);
			if (!alive || bgDeleting !== id) return;
			delFailures = 0;
			if (cur?.id === id) cur = j;
			if (j.status === 'deleting') {
				delTimer = setTimeout(() => pollDelete(id), DELETE_POLL_MS);
				return;
			}
			bgDeleting = null;
			toast(`The delete stopped: the job is ${j.status}.`, 'err');
		} catch (e) {
			if (!alive || bgDeleting !== id) return;
			delFailures += 1;
			const step = afterPollError(e, delFailures, DELETE_POLL_MS);
			if (step.kind === 'retry') {
				delTimer = setTimeout(() => pollDelete(id), step.delay);
				return;
			}
			bgDeleting = null;
			if (step.kind === 'gone') {
				toast('Job deleted.');
				if (cur?.id === id) {
					open = false;
					cur = null;
				}
			} else
				toast(
					step.kind === 'auth'
						? step.message
						: `Lost track of the delete: ${step.message}. Refresh to see where it is.`,
					'err',
				);
		}
		jobs.refresh();
	}

	/** Opens a page job's parent crawl with its delete confirm (a page job is deleted only with it). */
	async function openParent(id: string) {
		try {
			const p = await api().ingestJob(id);
			cur = p;
			open = true;
			confirmCancel = false;
			refusal = null;
			refusedParent = null;
			confirmDelete = true;
		} catch (e) {
			toast(`Could not open the parent crawl: ${(e as Error).message}`, 'err');
		}
	}
</script>

<header class="page-head">
	<div>
		<h1>Ingest jobs</h1>
		<p>What strata_ingest did with each source: steps, providers, models and cost.</p>
	</div>
	<div class="actions">
		<Freshness resource={fresh} />
		<a class="btn" href={jobPath(app.job, '/knowledge')}>Sources</a>
	</div>
</header>

{#if stores.value.state === 'unavailable'}
	<Unavailable reason={`Stores: ${stores.value.reason}`} retry={() => stores.refresh()} />
{/if}

<div class="row">
	<label class="field"
		><span class="label">Status</span>
		<select class="input" bind:value={status}>
			<option value="">All</option>
			{#each STATUSES as s}<option value={s}>{s}</option>{/each}
		</select></label
	>
	<label class="field"
		><span class="label">Store</span>
		<select class="input" bind:value={store}>
			<option value="">All</option>
			{#if stores.value.state === 'ok'}
				{#each stores.value.data.items.filter((s) => s.kind === 'leaf') as s}<option value={s.id}
						>{s.name}</option
					>{/each}
			{/if}
		</select></label
	>
</div>

{#if jobs.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if jobs.value.state === 'unavailable'}
	<Unavailable reason={jobs.value.reason} retry={() => jobs.refresh()} />
{:else if jobs.value.state === 'empty'}
	<div class="empty">No ingest jobs match.</div>
{:else}
	{#if jobs.value.data.next}
		<p class="muted" role="status">Showing the first {LIMIT} jobs.</p>
	{/if}
	{#if polling}
		<p class="muted" role="status">Jobs in progress: this list refreshes every {POLL_MS / 1000} seconds.</p>
	{/if}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead
					><tr
						><th>Created</th><th>Store</th><th>Source</th><th>Status</th><th class="r">Steps</th><th class="r"
							>Cost</th
						></tr
					></thead
				>
				<tbody>
					{#each jobs.value.data.items as j}
						<tr
							><td
								><button
									type="button"
									class="table-action"
									onclick={() => (
										(cur = j),
										(confirmCancel = false),
										(confirmDelete = false),
										(refusal = null),
										(refusedParent = null),
										(open = true)
									)}>{when(j.created_at)}</button
								></td
							>
							<td class="mono">{storeName(j.store_id)}</td>
							<td class="mono">{jobSourceLabel(j)}</td>
							<td><Status word={j.status} /></td>
							<td class="r">{j.steps.length}</td>
							<td class="r">{euros(jobCost(j))}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

<Drawer bind:open title={cur ? jobSourceLabel(cur) : ''} wide>
	{#if cur}
		<p class="muted">
			<Status word={cur.status} /> · {cur.document_ids.length} documents · total {euros(jobCost(cur))}
		</p>
		{#if pages}<p class="cell-sub">Page jobs: {childrenSummary(pages)}</p>{/if}
		{#if cur.error}<p class="cell-sub">{errText(cur.error)}</p>{/if}
		{#if parentId}
			<p class="cell-sub" data-testid="page-job">
				A page job of crawl <span class="mono">{parentId.slice(0, 8)}</span>: it is deleted with its parent.
			</p>
		{/if}
		{#if cur.status === 'deleting' && bgDeleting === cur.id}
			<div class="callout gap" role="status">
				<strong>Deleting…</strong>
				<span
					>strata_ingest is finishing the delete in the background. The job disappears when it is done.</span
				>
			</div>
		{:else if cur.status === 'deleting'}
			<div class="callout gap" role="status">
				<strong>Partly deleted — retry to finish</strong>
				<span
					>A delete of this job did not finish (storage could not remove a file). Some of its files are
					already gone, so it can no longer run or be retried: Retry delete to finish it.</span
				>
			</div>
		{/if}
		<div class="tbl-wrap">
			<table class="tbl dense">
				<thead
					><tr
						><th>Step</th><th>Provider</th><th>Model</th><th>Status</th><th class="r">Attempt</th><th
							class="r">Cost</th
						></tr
					></thead
				>
				<tbody>
					{#each cur.steps as st}
						<tr>
							<td>{st.kind}<span class="cell-sub mono">{st.key}</span></td>
							<td>{st.provider ?? '—'}</td>
							<td class="mono">{st.model ?? '—'}</td>
							<td
								><Status word={st.status} />{#if st.error}<span class="cell-sub">{errText(st.error)}</span
									>{/if}</td
							>
							<td class="r">{st.attempt}</td>
							<td class="r">{euros(st.cost_micros)}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		{#if confirmCancel}
			<div class="callout err" role="alertdialog" aria-label="Cancel job">
				<strong>Cancel this job?</strong>
				<span
					>Work already done stays; the unfinished steps are not run. A step in flight finishes first (the job
					shows cancelling until then).</span
				>
				<span
					><button class="btn sm" type="button" onclick={() => act('cancel')}>Confirm cancel</button>
					<button class="btn sm" type="button" onclick={() => (confirmCancel = false)}>Keep running</button
					></span
				>
			</div>
		{/if}
		{#if confirmDelete}
			<div class="callout err" role="alertdialog" aria-label="Delete job">
				<strong>Delete this job?</strong>
				<span
					>Its record and staged files in strata_ingest are removed{cur.child_job_ids?.length
						? `, with its ${cur.child_job_ids.length} page jobs`
						: ''}. The {cur.document_ids.length} document{cur.document_ids.length === 1 ? '' : 's'} it delivered
					stay in the knowledge store: delete them under Stores if they should go too.</span
				>
				{#if refusal}<span role="alert" data-testid="delete-refusal">Not deleted: {refusal}</span>{/if}
				{#if refusedParent}<span
						><button class="btn sm" type="button" onclick={() => refusedParent && openParent(refusedParent)}
							>Open the parent crawl</button
						></span
					>{/if}
				<span
					><button class="btn sm" type="button" onclick={remove}>Delete job</button>
					<button class="btn sm" type="button" onclick={() => (confirmDelete = false)}>Keep it</button></span
				>
			</div>
		{/if}
		<div class="actions">
			<button
				class="btn"
				type="button"
				disabled={!canWrite || cur.status !== 'failed'}
				title={why || (cur.status !== 'failed' ? 'Only a failed job can be retried' : '')}
				onclick={() => act('retry')}>Retry</button
			>
			<button
				class="btn"
				type="button"
				disabled={!canWrite || !cancellable}
				title={why || cancelWhy}
				onclick={() => ((confirmCancel = true), (confirmDelete = false))}>Cancel job</button
			>
			{#if parentId}
				<button
					class="btn"
					type="button"
					disabled={!canWrite}
					title={why || 'A page job is deleted only with its crawl: open the parent to delete it'}
					onclick={() => parentId && openParent(parentId)}>Delete the parent crawl</button
				>
			{:else}
				<button
					class="btn"
					type="button"
					disabled={!canWrite || !deletable}
					title={why || deleteWhy}
					onclick={() => (
						(confirmDelete = true),
						(confirmCancel = false),
						(refusal = null),
						(refusedParent = null)
					)}>{cur.status === 'deleting' && bgDeleting !== cur.id ? 'Retry delete' : 'Delete job'}</button
				>
			{/if}
		</div>
	{/if}
</Drawer>
