<script lang="ts">
	// One research run (plan 18): status, the verified memo, its claims with their citations, and the step timeline.
	// A run that is not finished is read again every 3 s until it is; a failed read backs off ($lib/poll).
	import { untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { afterPollError } from '$lib/poll';
	import { fmt } from '$lib/format';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		LEGAL_BANNER,
		PROFILE_LABEL,
		RESEARCH_POLL_MS,
		STATUS_LABEL,
		VERIFY_LABEL,
		citationHref,
		claimsWithCitations,
		isTerminal,
		statusWord,
		verifyDetailText,
		verifyVariant,
		when,
		type ResearchRun,
		type ResearchRunDetail,
	} from '$lib/api/research';
	import Callout from '$lib/ui/Callout.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	let {
		id,
		canWrite = false,
		onchange,
	}: {
		id: string;
		/** Any member of the tenant may cancel; false hides nothing but disables Cancel. */
		canWrite?: boolean;
		/** Called with the run after every read and cancel, so the list row keeps up. */
		onchange?: (run: ResearchRun) => void;
	} = $props();

	let detail = $state<ResearchRunDetail | null>(null);
	let loadError = $state('');
	/** Why following the run stopped before it ended. */
	let pollErr = $state('');
	let cancelling = $state(false);
	let cancelError = $state('');
	let failures = 0;
	let timer: ReturnType<typeof setTimeout> | undefined;
	/** Bumped per id: a read for another (or a closed) run is dropped. */
	let gen = 0;

	async function read(g: number) {
		let d: ResearchRunDetail;
		try {
			d = await api().researchRun(id);
		} catch (e) {
			if (g !== gen) return;
			if (!detail) {
				loadError = e instanceof Error ? e.message : String(e);
				return;
			}
			failures += 1;
			const step = afterPollError(e, failures, RESEARCH_POLL_MS);
			if (step.kind === 'retry') {
				timer = setTimeout(() => read(g), step.delay);
				return;
			}
			pollErr =
				step.kind === 'gone'
					? 'This run no longer exists.'
					: step.kind === 'auth'
						? step.message
						: `Stopped following this run after ${failures} failed reads: ${step.message}`;
			return;
		}
		if (g !== gen) return;
		failures = 0;
		detail = d;
		loadError = '';
		pollErr = '';
		onchange?.(d.run);
		if (!isTerminal(d.run.status)) timer = setTimeout(() => read(g), RESEARCH_POLL_MS);
	}

	function start() {
		clearTimeout(timer);
		const g = ++gen;
		failures = 0;
		detail = null;
		loadError = '';
		pollErr = '';
		cancelError = '';
		void read(g);
	}

	function retryFollow() {
		clearTimeout(timer);
		failures = 0;
		pollErr = '';
		void read(gen);
	}

	$effect(() => {
		void id;
		untrack(start);
		return () => {
			gen += 1;
			clearTimeout(timer);
		};
	});

	async function cancel() {
		if (!detail || cancelling || !canWrite) return;
		cancelling = true;
		cancelError = '';
		try {
			const run = await api().cancelResearchRun(detail.run.id);
			detail = { ...detail, run };
			onchange?.(run);
			toast(
				run.status === 'cancelled' ? 'Run cancelled.' : 'Cancel requested; the run stops after its step.',
			);
		} catch (e) {
			cancelError =
				e instanceof ApiError && e.code === 'run_finished'
					? 'This run has already finished.'
					: e instanceof Error
						? e.message
						: String(e);
			retryFollow();
		} finally {
			cancelling = false;
		}
	}

	const claims = $derived(detail ? claimsWithCitations(detail) : []);
	const budgetText = (b: Record<string, unknown>) =>
		Object.keys(b).length
			? Object.entries(b)
					.map(([k, v]) => `${k.replace('max_', '')} ${v}`)
					.join(' · ')
			: 'runner defaults';
</script>

{#if loadError}
	<Unavailable reason={loadError} retry={start} />
{:else if !detail}
	<p class="muted">Loading…</p>
{:else}
	{@const r = detail.run}
	{#if r.profile === 'legal'}
		<div class="callout gap rs-legal" role="note" aria-label="Legal draft notice">
			<strong>{LEGAL_BANNER}</strong>
		</div>
	{/if}
	<div class="rs-head">
		<Status word={statusWord(r.status)} label={STATUS_LABEL[r.status]} />
		{#if r.cancel_requested && !isTerminal(r.status)}<Chip text="cancel requested" variant="warn" />{/if}
		{#if !isTerminal(r.status)}
			<span class="muted" data-testid="following">Updating every {RESEARCH_POLL_MS / 1000} s</span>
			<button
				class="btn sm danger"
				type="button"
				disabled={!canWrite || cancelling || r.cancel_requested}
				title={canWrite ? '' : 'Needs a role in this tenant'}
				onclick={cancel}>{cancelling ? 'Cancelling…' : 'Cancel run'}</button
			>
		{/if}
	</div>
	{#if pollErr}
		<div class="callout err" role="alert" data-testid="poll-error">
			<span>{pollErr}</span>
			<button class="btn sm" type="button" onclick={retryFollow}>Follow again</button>
		</div>
	{/if}
	{#if cancelError}<Callout kind="err" title="Not cancelled">{cancelError}</Callout>{/if}
	{#if r.error}<Callout kind="err" title="Run failed">{r.error}</Callout>{/if}
	<KeyValues
		pairs={[
			['Profile', PROFILE_LABEL[r.profile] ?? r.profile],
			['Steps', fmt.n(r.steps_used)],
			['Tokens', fmt.n(r.tokens_used)],
			['Attempts', fmt.n(r.attempts)],
			['Limits', budgetText(r.budget as Record<string, unknown>)],
			['Stores', r.store_ids.length ? String(r.store_ids.length) : 'none'],
			['Created', `${when(r.created_at)} · ${r.created_by}`],
			['Started', when(r.started_at)],
			['Finished', when(r.finished_at)],
		]}
	/>
	<h3>Question</h3>
	<p class="rs-pre">{r.question}</p>

	<h3>Report</h3>
	{#if r.report}
		<!-- No markdown renderer in this app: the memo is shown as its markdown text. -->
		<pre class="rs-report" data-testid="report">{r.report}</pre>
	{:else}
		<p class="muted">
			{isTerminal(r.status) ? 'No report.' : 'The report is written when the run finishes.'}
		</p>
	{/if}

	<h3>Claims</h3>
	{#if claims.length === 0}
		<p class="muted">No claims yet.</p>
	{:else}
		<ol class="rs-claims" aria-label="Claims">
			{#each claims as { claim, citations } (claim.id)}
				<li>
					<div class="rs-claim-head">
						<span>{claim.text}</span>
						<Status
							word={claim.status === 'supported' ? 'ok' : claim.status === 'unsupported' ? 'err' : 'queued'}
							label={claim.status}
						/>
					</div>
					{#if citations.length === 0}
						<p class="muted">No citations.</p>
					{:else}
						<ul class="rs-cites">
							{#each citations as c (c.id)}
								{@const href = citationHref(c)}
								{@const detailText = verifyDetailText(c.verify_detail)}
								<li class="rs-cite">
									<div class="rs-cite-head">
										<Chip text={c.source_kind} variant="accent" />
										{#if href}<a class="mono" {href} target="_blank" rel="noopener noreferrer">{c.locator}</a
											>{:else}<span class="mono">{c.locator}</span>{/if}
										{#if c.title}<span class="muted">{c.title}</span>{/if}
										<Chip
											text={VERIFY_LABEL[c.verify_status] ?? c.verify_status}
											variant={verifyVariant(c.verify_status)}
											title={detailText || undefined}
										/>
									</div>
									<blockquote>{c.quote}</blockquote>
								</li>
							{/each}
						</ul>
					{/if}
				</li>
			{/each}
		</ol>
	{/if}

	<details class="rs-steps">
		<summary>Steps ({detail.steps.length})</summary>
		{#if detail.steps.length === 0}
			<p class="muted">No steps yet.</p>
		{:else}
			<div class="tbl-wrap">
				<table class="tbl" aria-label="Steps">
					<thead>
						<tr><th class="r">#</th><th>Kind</th><th>Tool</th><th>Status</th><th class="r">Duration</th></tr>
					</thead>
					<tbody>
						{#each detail.steps as s (s.ordinal)}
							<tr>
								<td class="r">{s.ordinal}</td>
								<td>{s.kind}</td>
								<td class="mono">{s.tool ?? '—'}</td>
								<td
									><Status
										word={s.status === 'denied' ? 'warn' : s.status === 'error' ? 'err' : 'ok'}
										label={s.status}
									/></td
								>
								<td class="r">{fmt.ms(s.duration_ms)}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</details>
{/if}

<style>
	.rs-legal {
		margin-bottom: 12px;
	}
	.rs-head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 10px;
		margin-bottom: 12px;
	}
	.rs-pre,
	.rs-report {
		white-space: pre-wrap;
		overflow-wrap: anywhere;
	}
	.rs-report {
		font: inherit;
		margin: 0;
		padding: 12px;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--panel);
	}
	.rs-claims {
		display: grid;
		gap: 12px;
		padding-left: 20px;
	}
	.rs-claim-head,
	.rs-cite-head {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
	}
	.rs-cites {
		display: grid;
		gap: 8px;
		list-style: none;
		padding: 0;
		margin: 8px 0 0;
	}
	.rs-cite a,
	.rs-cite .mono {
		overflow-wrap: anywhere;
	}
	.rs-cite blockquote {
		margin: 4px 0 0;
		padding-left: 10px;
		border-left: 3px solid var(--line);
		color: var(--muted);
		white-space: pre-wrap;
	}
	.rs-steps {
		margin-top: 16px;
	}
	.rs-steps summary {
		cursor: pointer;
		font-weight: 600;
	}
</style>
