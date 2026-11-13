<script lang="ts">
	import { onDestroy } from 'svelte';
	import { api, guard } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { session } from '$lib/auth/session.svelte';
	import { Latest, Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { storeTree } from '$lib/sources';
	import { afterPollError } from '$lib/poll';
	import {
		EVAL_MAX_QUESTIONS,
		EVAL_MODES,
		EVAL_POLL_MS,
		MODE_LABEL,
		compare,
		delta,
		emptyForm,
		expectations,
		formOf,
		formProblem,
		grid,
		inputOf,
		pct,
		progressLabel,
		rankLabel,
		runChip,
		withRun,
		type QuestionForm,
	} from '$lib/eval';
	import type {
		EvalMode,
		EvalProgress,
		EvalQuestion,
		EvalRun,
		EvalRunSummary,
		Paged,
		StoreNode,
	} from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const questions = new Resource<Paged<EvalQuestion>>(() => api().evalQuestions());
	const stores = new Resource<Paged<StoreNode>>(() => api().stores());
	questions.refresh();
	stores.refresh();
	const canWrite = $derived(session.can('tenant_admin'));
	const why = $derived(canWrite ? '' : 'Needs the tenant_admin role');
	const qItems = $derived(questions.value.state === 'ok' ? questions.value.data.items : []);

	// ---- questions: add / edit / delete / import ----
	let form = $state<QuestionForm>(emptyForm());
	let editing = $state<EvalQuestion | null>(null);
	let formOpen = $state(false);
	let saving = $state(false);
	let formErr = $state('');
	let confirmDel = $state<EvalQuestion | null>(null);
	const problem = $derived(formProblem(form));

	function openNew() {
		editing = null;
		form = emptyForm();
		formErr = '';
		formOpen = true;
	}
	function openEdit(q: EvalQuestion) {
		editing = q;
		form = formOf(q);
		formErr = '';
		formOpen = true;
	}
	async function save() {
		if (problem) return;
		saving = true;
		formErr = '';
		try {
			const input = inputOf(form);
			if (editing) await api().updateEvalQuestion(editing.id, input);
			else await api().createEvalQuestion(input);
			toast(editing ? 'Question saved.' : 'Question added.');
			formOpen = false;
			editing = null;
			form = emptyForm();
			questions.refresh();
		} catch (e) {
			// question_exists / invalid_input / too_many_questions: strata_rag's message says which
			formErr = (e as Error).message;
		}
		saving = false;
	}
	async function remove(q: EvalQuestion) {
		confirmDel = null;
		const ok = await guard(
			() => api().deleteEvalQuestion(q.id),
			'Question deleted. Earlier runs keep it.',
			'Not deleted',
		);
		if (ok) questions.refresh();
	}

	let file = $state<File | null>(null);
	let importing = $state(false);
	let importErr = $state('');
	async function importCsv() {
		if (!file) return;
		importing = true;
		importErr = '';
		try {
			const r = await api().importEvalQuestions(file);
			toast(`Imported ${r.added} question${r.added === 1 ? '' : 's'}; the tenant now has ${r.total}.`);
			file = null;
			questions.refresh();
		} catch (e) {
			importErr = (e as Error).message;
		}
		importing = false;
	}

	// ---- run ----
	let rootId = $state('');
	let modes = $state<Record<EvalMode, boolean>>({ embed: true, laya: false, hybrid: true, flat: false });
	// bind:value on a number input gives a number, or null when cleared
	let beam = $state<number | null>(null);
	let minScore = $state<number | null>(null);
	let starting = $state(false);
	let runErr = $state<{ code: string; message: string } | null>(null);
	const picked = $derived(EVAL_MODES.filter((m) => modes[m]));
	const storeItems = $derived(stores.value.state === 'ok' ? storeTree(stores.value.data.items) : []);
	const storeName = (id: string | null) => {
		if (!id) return 'Whole tree';
		const s = stores.value.state === 'ok' ? stores.value.data.items.find((x) => x.id === id) : undefined;
		return s?.name ?? id.slice(0, 8);
	};
	async function run() {
		starting = true;
		runErr = null;
		const num = (v: number | null) => (v == null || !Number.isFinite(Number(v)) ? undefined : Number(v));
		try {
			const r = await api().runEval({
				...(rootId ? { root_id: rootId } : {}),
				modes: picked,
				beam: num(beam),
				min_score: num(minScore),
			});
			// 202: strata_rag scores it in the background; follow it here (and from the runs list after a reload)
			runPages = [r, ...runPages.filter((x) => x.id !== r.id)];
			watch(r.id, r.progress ?? null);
			open(r);
			runs.refresh();
		} catch (e) {
			runErr = {
				code: e instanceof ApiError ? e.code : 'error',
				message: (e as Error).message,
			};
		}
		starting = false;
	}

	// ---- following a running run: poll it until it is completed or failed ----
	let watching = $state<string | null>(null);
	let progress = $state<EvalProgress | null>(null);
	/** Why following a run stopped before it ended; `retry` names the run when following it again can help. */
	let pollErr = $state<{ message: string; retry: string | null } | null>(null);
	/** Failed reads in a row of the followed run. */
	let failures = 0;
	let timer: ReturnType<typeof setTimeout> | undefined;
	let alive = true;
	onDestroy(() => {
		alive = false;
		clearTimeout(timer);
	});
	const busy = $derived(starting || watching !== null);
	function watch(id: string, p: EvalProgress | null = null) {
		if (watching === id) return;
		clearTimeout(timer);
		watching = id;
		progress = p;
		pollErr = null;
		failures = 0;
		timer = setTimeout(() => poll(id), EVAL_POLL_MS);
	}
	async function poll(id: string) {
		if (!alive || watching !== id) return;
		let r: EvalRun;
		try {
			r = await api().evalRun(id);
		} catch (e) {
			if (!alive || watching !== id) return;
			failures += 1;
			const step = afterPollError(e, failures, EVAL_POLL_MS);
			// a missed poll (network, a restart) is not the run's outcome: try again, waiting longer each time
			if (step.kind === 'retry') {
				timer = setTimeout(() => poll(id), step.delay);
				return;
			}
			// gone (deleted, or another tenant is active now), signed out / no permission, or too many failures:
			// stop following it, so Run is usable again
			watching = null;
			progress = null;
			pollErr =
				step.kind === 'gone'
					? { message: 'This run no longer exists.', retry: null }
					: step.kind === 'auth'
						? { message: step.message, retry: null }
						: {
								message: `Lost track of this run after ${failures} failed reads: ${step.message}`,
								retry: id,
							};
			if (step.kind === 'gone') runs.refresh();
			return;
		}
		failures = 0;
		if (!alive || watching !== id) return;
		progress = r.progress ?? null;
		runPages = runPages.map((x) => (x.id === id ? withRun(x, r) : x));
		if (detail.head?.id === id) detail = { s: 'ok', head: r, full: r, err: '' };
		if (r.status === 'running') {
			timer = setTimeout(() => poll(id), EVAL_POLL_MS);
			return;
		}
		watching = null;
		if (r.status === 'failed') toast(`Eval failed: ${r.error ?? 'strata_rag gave no reason'}`, 'err');
		else toast(`Eval finished: ${r.verdict}.`);
		runs.refresh();
	}

	// ---- runs list, detail, compare ----
	let runPages = $state<EvalRunSummary[]>([]);
	let runsNext = $state<string | null>(null);
	let loadingMore = $state(false);
	const runs = new Resource<Paged<EvalRunSummary>>(async () => {
		const p = await api().evalRuns();
		runPages = p.items;
		runsNext = p.next;
		// a run started earlier (this page left and come back to) is followed again
		const live = p.items.find((r) => r.status === 'running');
		if (live) watch(live.id, live.progress ?? null);
		return p;
	});
	runs.refresh();
	async function moreRuns() {
		if (!runsNext || loadingMore) return;
		loadingMore = true;
		try {
			const p = await api().evalRuns({ before: runsNext });
			runPages = [...runPages, ...p.items];
			runsNext = p.next;
		} catch (e) {
			toast(`Could not load more runs: ${(e as Error).message}`, 'err');
		}
		loadingMore = false;
	}

	/** `head` is what is known at once (a listed summary or a full run); `full` the run with questions and results. */
	let detail = $state<{
		s: 'idle' | 'loading' | 'ok' | 'err';
		head: EvalRunSummary | EvalRun | null;
		full: EvalRun | null;
		err: string;
	}>({ s: 'idle', head: null, full: null, err: '' });
	const latest = new Latest();
	const countOf = (r: EvalRunSummary | EvalRun) => ('questions' in r ? r.questions.length : r.question_count);
	async function open(r: EvalRunSummary | EvalRun) {
		const ticket = latest.next();
		if ('results' in r && r.status !== 'running') {
			detail = { s: 'ok', head: r, full: r, err: '' };
			return;
		}
		detail = { s: 'loading', head: r, full: null, err: '' };
		try {
			const full = await api().evalRun(r.id);
			if (!latest.stale(ticket)) detail = { s: 'ok', head: full, full, err: '' };
		} catch (e) {
			if (!latest.stale(ticket)) detail = { s: 'err', head: r, full: null, err: (e as Error).message };
		}
	}

	let chosen = $state<string[]>([]);
	function toggle(id: string) {
		chosen = chosen.includes(id) ? chosen.filter((x) => x !== id) : [...chosen, id].slice(-2);
	}
	let cmp = $state<{
		s: 'idle' | 'loading' | 'ok' | 'err';
		a: EvalRun | null;
		b: EvalRun | null;
		err: string;
	}>({ s: 'idle', a: null, b: null, err: '' });
	const cmpLatest = new Latest();
	async function compareChosen() {
		if (chosen.length !== 2) return;
		const ticket = cmpLatest.next();
		cmp = { s: 'loading', a: null, b: null, err: '' };
		try {
			const [x, y] = await Promise.all(chosen.map((id) => api().evalRun(id)));
			// A = the older run, B = the newer one
			const [a, b] = x!.started_at <= y!.started_at ? [x!, y!] : [y!, x!];
			if (!cmpLatest.stale(ticket)) cmp = { s: 'ok', a, b, err: '' };
		} catch (e) {
			if (!cmpLatest.stale(ticket)) cmp = { s: 'err', a: null, b: null, err: (e as Error).message };
		}
	}
	const comparison = $derived(cmp.s === 'ok' && cmp.a && cmp.b ? compare(cmp.a, cmp.b) : null);
	const when = (t: string) =>
		new Date(t).toLocaleString('en-GB', { dateStyle: 'medium', timeStyle: 'short' });
</script>

<header class="page-head">
	<div>
		<h1>RAG eval</h1>
		<p>
			Golden questions for this tenant's knowledge, scored by strata_rag as recall@1/5/10 per search mode.
			Every run is kept, so a change to sources, stores or routing can be compared with the run before it.
		</p>
	</div>
	<div class="actions"><Freshness resource={questions} /></div>
</header>

<Card
	title="Questions"
	sub={questions.value.state === 'ok' || questions.value.state === 'empty'
		? `${qItems.length} of ${EVAL_MAX_QUESTIONS}`
		: undefined}
>
	{#snippet actions()}
		<button class="btn sm primary" type="button" disabled={!canWrite} title={why} onclick={openNew}
			>Add question</button
		>
	{/snippet}
	{#if formOpen}
		<form
			class="form-grid q-form"
			aria-label={editing ? 'Edit question' : 'New question'}
			onsubmit={(e) => {
				e.preventDefault();
				save();
			}}
		>
			<label class="field wide"
				><span class="label">Question</span><input
					class="input"
					bind:value={form.query}
					placeholder="How long does a refund take?"
				/></label
			>
			<label class="field"
				><span class="label">Expected document</span><input
					class="input"
					bind:value={form.expected_document}
					placeholder="Title, source or file name"
				/></label
			>
			<label class="field"
				><span class="label">Expected text</span><input
					class="input"
					bind:value={form.expected_text}
					placeholder="Words the passage contains"
				/></label
			>
			<label class="field"
				><span class="label">Expected leaf store</span><input
					class="input"
					bind:value={form.expected_leaf}
					placeholder="Leaf name, path a/b or id (optional)"
				/></label
			>
			<label class="field"
				><span class="label">Document ids</span><textarea
					class="input"
					rows="2"
					bind:value={form.expected_document_ids}
				></textarea></label
			>
			<label class="field"
				><span class="label">Chunk ids</span><textarea
					class="input"
					rows="2"
					bind:value={form.expected_chunk_ids}
				></textarea></label
			>
			<p class="muted wide">
				A result counts as a hit when it matches any expectation. The leaf store only scores routing.
			</p>
			{#if formErr}<div class="callout err wide" role="alert">{formErr}</div>{/if}
			<div class="row wide">
				<button class="btn primary" type="submit" disabled={!!problem || saving} title={problem ?? ''}
					>{saving ? 'Saving…' : editing ? 'Save question' : 'Add question'}</button
				>
				<button class="btn" type="button" onclick={() => (formOpen = false)}>Cancel</button>
				{#if problem}<span class="muted">{problem}</span>{/if}
			</div>
		</form>
	{/if}
	{#if confirmDel}
		<div class="callout err" role="alertdialog" aria-label="Delete question">
			<strong>Delete “{confirmDel.query}”?</strong>
			<span>It is no longer scored. Runs that already scored it keep it.</span>
			<span
				><button class="btn sm" type="button" onclick={() => remove(confirmDel!)}>Delete question</button>
				<button class="btn sm" type="button" onclick={() => (confirmDel = null)}>Keep it</button></span
			>
		</div>
	{/if}
	{#if questions.value.state === 'unavailable'}
		<Unavailable reason={questions.value.reason} retry={questions.retry} />
	{:else if questions.value.state === 'empty'}
		<div class="empty">No questions yet. Add one, or import a CSV.</div>
	{:else if questions.value.state === 'loading'}
		<p class="muted">Loading…</p>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl" aria-label="Eval questions">
				<thead><tr><th>Question</th><th>A hit is</th><th>Leaf</th><th></th></tr></thead>
				<tbody>
					{#each qItems as q (q.id)}
						<tr>
							<td>{q.query}</td>
							<td class="muted">{expectations(q).join(' or ')}</td>
							<td class="mono">{q.expected_leaf ?? '—'}</td>
							<td class="r nowrap">
								<button
									class="btn sm"
									type="button"
									disabled={!canWrite}
									title={why}
									aria-label="Edit {q.query}"
									onclick={() => openEdit(q)}>Edit</button
								>
								<button
									class="btn sm"
									type="button"
									disabled={!canWrite}
									title={why}
									aria-label="Delete {q.query}"
									onclick={() => (confirmDel = q)}>Delete</button
								>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
	<form
		class="row import"
		onsubmit={(e) => {
			e.preventDefault();
			importCsv();
		}}
	>
		<label class="field"
			><span class="label">Import CSV</span><input
				class="input"
				type="file"
				accept=".csv,text/csv,text/plain"
				disabled={!canWrite}
				onchange={(e) => (file = (e.currentTarget as HTMLInputElement).files?.[0] ?? null)}
			/></label
		>
		<button class="btn" type="submit" disabled={!canWrite || !file || importing} title={why}
			>{importing ? 'Importing…' : 'Import'}</button
		>
		<span class="muted"
			>Header row with <code>query</code> and any of <code>expected_document</code>,
			<code>expected_text</code>, <code>expected_document_ids</code>, <code>expected_chunk_ids</code>,
			<code>expected_leaf</code>; ids separated by <code>;</code>. All rows or none.</span
		>
	</form>
	{#if importErr}<div class="callout err" role="alert">
			<strong>Nothing imported</strong><span>{importErr}</span>
		</div>{/if}
</Card>

<Card title="Run eval" sub="POST /api/knowledge/eval/runs → strata_rag POST /api/eval (source tenant)">
	<form
		class="row run"
		onsubmit={(e) => {
			e.preventDefault();
			run();
		}}
	>
		<label class="field"
			><span class="label">Store</span>
			<select class="input" bind:value={rootId} aria-label="Store">
				<option value="">Whole tree</option>
				{#each storeItems as { node, depth } (node.id)}
					<option value={node.id}>{'  '.repeat(depth)}{node.name} ({node.kind})</option>
				{/each}
			</select></label
		>
		<fieldset class="modes">
			<legend class="label">Modes</legend>
			{#each EVAL_MODES as m (m)}
				<label class="check"><input type="checkbox" bind:checked={modes[m]} /> {MODE_LABEL[m]}</label>
			{/each}
		</fieldset>
		<label class="field" title="How many router branches each query follows (empty = strata_rag's default)"
			><span class="label">Branches (beam)</span><input
				class="input"
				type="number"
				min="1"
				max="8"
				placeholder="default"
				bind:value={beam}
			/></label
		>
		<label class="field" title="Passages scoring below this (0 to 1) are left out (empty = default)"
			><span class="label">Minimum score</span><input
				class="input"
				type="number"
				min="0"
				max="1"
				step="0.05"
				placeholder="default"
				bind:value={minScore}
			/></label
		>
		<button
			class="btn primary"
			type="submit"
			disabled={!canWrite || busy || picked.length === 0 || qItems.length === 0}
			title={why ||
				(watching
					? 'An eval is running; one at a time per tenant'
					: picked.length === 0
						? 'Pick at least one mode'
						: qItems.length === 0
							? 'Add questions first'
							: '')}>{starting ? 'Starting…' : watching ? 'Running…' : 'Run eval'}</button
		>
	</form>
	{#if watching}
		<p class="muted" role="status" data-testid="run-progress">
			Running{progress ? `: ${progressLabel(progress)}` : '…'}. strata_rag scores it in the background (laya
			and hybrid call laya for every question), so you can leave this page and come back; the run stays in the
			list.
		</p>
	{/if}
	{#if pollErr}
		<div class="callout err" role="alert" data-testid="poll-error">
			<span>{pollErr.message}</span>
			{#if pollErr.retry}<span
					><button class="btn sm" type="button" onclick={() => pollErr?.retry && watch(pollErr.retry)}
						>Retry</button
					></span
				>{/if}
		</div>
	{/if}
	{#if runErr}
		<div class="callout err" role="alert">
			<strong
				>{runErr.code === 'eval_not_ready'
					? 'Not ready to evaluate'
					: runErr.code === 'eval_running'
						? 'An eval is already running'
						: 'Eval failed'}</strong
			>
			<span class="pre">{runErr.message}</span>
		</div>
	{/if}
</Card>

<Card title="Runs" sub="Newest first. Tick two to compare.">
	{#snippet actions()}
		<button
			class="btn sm"
			type="button"
			disabled={chosen.length !== 2 || cmp.s === 'loading'}
			onclick={compareChosen}>Compare selected</button
		>
	{/snippet}
	{#if runs.value.state === 'unavailable'}
		<Unavailable reason={runs.value.reason} retry={runs.retry} />
	{:else if runs.value.state === 'empty'}
		<div class="empty">No eval runs yet.</div>
	{:else if runs.value.state === 'loading'}
		<p class="muted">Loading…</p>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl dense" aria-label="Eval runs">
				<thead>
					<tr>
						<th></th><th>Started</th><th>Store</th><th class="r">Questions</th><th>Verdict</th>
						<th>Recall@1 / @5 / @10 per mode</th><th></th>
					</tr>
				</thead>
				<tbody>
					{#each runPages as r (r.id)}
						<tr class:on={detail.head?.id === r.id}>
							<td
								><input
									type="checkbox"
									aria-label="Compare run {when(r.started_at)}"
									disabled={r.status !== 'completed'}
									checked={chosen.includes(r.id)}
									onchange={() => toggle(r.id)}
								/></td
							>
							<td class="nowrap">{when(r.started_at)}</td>
							<td>{storeName(r.root_id)}</td>
							<td class="r">{r.question_count}</td>
							<td>
								<Chip text={runChip(r).text} variant={runChip(r).variant} title={r.error ?? undefined} />
							</td>
							<td>
								{#each r.reports ?? [] as rep (rep.mode)}
									<span class="rec mono"
										><b>{rep.mode}</b>
										{pct(rep.recall_at_1)} / {pct(rep.recall_at_5)} / {pct(rep.recall_at_10)}</span
									>
								{/each}
							</td>
							<td><button class="btn sm" type="button" onclick={() => open(r)}>Open</button></td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
		{#if runsNext}
			<button class="btn sm" type="button" disabled={loadingMore} onclick={moreRuns}
				>{loadingMore ? 'Loading…' : 'Load more'}</button
			>
		{/if}
	{/if}
</Card>

{#if detail.s !== 'idle' && detail.head}
	{@const r = detail.head}
	<Card
		title="Run {when(r.started_at)}"
		sub="{storeName(r.root_id)} · {countOf(
			r,
		)} questions · beam {r.beam} · min score {r.min_score}{r.took_ms == null
			? ''
			: ` · took ${(r.took_ms / 1000).toFixed(1)} s`}"
	>
		<div data-testid="run-detail">
			<p><Chip text={runChip(r).text} variant={runChip(r).variant} /></p>
			{#if r.status === 'running'}
				<p class="muted" role="status">
					Scoring{r.progress ? ` ${progressLabel(r.progress)}` : ''}. The results appear here when it
					completes.
				</p>
			{:else if r.status === 'failed'}
				<div class="callout err" role="alert">
					<strong>The eval failed</strong><span class="pre">{r.error ?? 'strata_rag gave no reason.'}</span>
				</div>
			{/if}
			{#if r.reports}
				<div class="tbl-wrap">
					<table class="tbl dense" aria-label="Recall per mode">
						<thead>
							<tr>
								<th>Mode</th><th class="r">Recall@1</th><th class="r">Recall@5</th><th class="r">Recall@10</th
								>
								<th class="r">Leaf accuracy</th><th class="r">Laya consulted</th><th class="r"
									>Laya fallbacks</th
								>
							</tr>
						</thead>
						<tbody>
							{#each r.reports ?? [] as rep (rep.mode)}
								<tr>
									<td>{MODE_LABEL[rep.mode] ?? rep.mode}</td>
									<td class="r">{pct(rep.recall_at_1)}</td>
									<td class="r">{pct(rep.recall_at_5)}</td>
									<td class="r">{pct(rep.recall_at_10)}</td>
									<td class="r">{pct(rep.leaf_accuracy)}</td>
									<td class="r">{rep.laya_consulted}</td>
									<td class="r">{rep.laya_fallbacks}</td>
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
			{#if detail.s === 'loading'}
				<p class="muted">Loading per-question results…</p>
			{:else if detail.s === 'err'}
				<Unavailable reason={detail.err} retry={() => open(r)} />
			{:else if detail.full && detail.full.status === 'completed'}
				{@const full = detail.full}
				<div class="tbl-wrap">
					<table class="tbl dense" aria-label="Per-question hits">
						<thead>
							<tr>
								<th>Question</th>
								{#each r.modes as m (m)}<th class="r">{m}</th>{/each}
							</tr>
						</thead>
						<tbody>
							{#each grid(full) as row (row.index)}
								<tr>
									<td
										>{row.question.query}<span class="cell-sub"
											>{expectations(row.question).join(' or ')}</span
										></td
									>
									{#each r.modes as m (m)}
										{@const c = row.cells[m]}
										<td class="r mono" class:miss={c?.rank == null}
											>{rankLabel(c?.rank)}{#if c?.leaf_hit != null}<span class="cell-sub"
													>{c.leaf_hit ? 'leaf ✓' : 'leaf ✗'}</span
												>{/if}</td
										>
									{/each}
								</tr>
							{/each}
						</tbody>
					</table>
				</div>
			{/if}
		</div>
	</Card>
{/if}

{#if cmp.s !== 'idle'}
	<Card title="Compare runs">
		{#if cmp.s === 'loading'}
			<p class="muted">Loading both runs…</p>
		{:else if cmp.s === 'err'}
			<Unavailable reason={cmp.err} retry={compareChosen} />
		{:else if comparison && cmp.a && cmp.b}
			<div data-testid="run-compare">
				<p class="muted">
					A = {when(cmp.a.started_at)} ({storeName(cmp.a.root_id)}), B = {when(cmp.b.started_at)} ({storeName(
						cmp.b.root_id,
					)}). {comparison.better} better, {comparison.worse} worse.
				</p>
				{#if comparison.modes.length === 0}
					<div class="empty">The two runs share no mode.</div>
				{:else}
					<div class="tbl-wrap">
						<table class="tbl dense" aria-label="Recall change">
							<thead>
								<tr
									><th>Mode</th><th class="r">@1 A → B</th><th class="r">@5 A → B</th><th class="r"
										>@10 A → B</th
									></tr
								>
							</thead>
							<tbody>
								{#each comparison.recall as d (d.mode)}
									<tr>
										<td>{MODE_LABEL[d.mode]}</td>
										<td class="r">{pct(d.a.recall_at_1)} → {pct(d.b.recall_at_1)} <b>{delta(d.d1)}</b></td>
										<td class="r">{pct(d.a.recall_at_5)} → {pct(d.b.recall_at_5)} <b>{delta(d.d5)}</b></td>
										<td class="r">{pct(d.a.recall_at_10)} → {pct(d.b.recall_at_10)} <b>{delta(d.d10)}</b></td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
					<div class="tbl-wrap">
						<table class="tbl dense" aria-label="Per-question change">
							<thead>
								<tr>
									<th>Question</th>
									{#each comparison.modes as m (m)}<th class="r">{m} A → B</th>{/each}
								</tr>
							</thead>
							<tbody>
								{#each comparison.rows as row, i (i)}
									<tr>
										<td>{row.query}</td>
										{#if row.only}
											<td class="muted" colspan={comparison.modes.length}
												>Only in run {row.only.toUpperCase()}</td
											>
										{:else}
											{#each comparison.modes as m (m)}
												{@const c = row.cells[m]}
												<td class="r mono {c?.change ?? ''}">{rankLabel(c?.a)} → {rankLabel(c?.b)}</td>
											{/each}
										{/if}
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
			</div>
		{/if}
	</Card>
{/if}

<style>
	.q-form {
		margin-bottom: 12px;
	}
	.wide {
		grid-column: 1 / -1;
	}
	.import {
		margin-top: 12px;
		align-items: end;
	}
	.run {
		align-items: end;
		flex-wrap: wrap;
	}
	.modes {
		border: 0;
		padding: 0;
		margin: 0;
		display: flex;
		gap: 10px;
		flex-wrap: wrap;
	}
	.check {
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.nowrap {
		white-space: nowrap;
	}
	.rec {
		display: block;
	}
	.pre {
		white-space: pre-wrap;
	}
	.miss {
		color: var(--muted);
	}
	.better {
		color: var(--ok);
	}
	.worse {
		color: var(--err);
	}
	tr.on td {
		background: var(--sunk);
	}
</style>
