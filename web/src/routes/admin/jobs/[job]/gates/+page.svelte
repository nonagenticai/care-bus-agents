<script lang="ts">
	import { env } from '$env/dynamic/public';
	import { api, app, guard, guardOutcome } from '$lib/app.svelte';
	import { getToken } from '$lib/auth/keycloak';
	import { session } from '$lib/auth/session.svelte';
	import { readConfig } from '$lib/config';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { ApiError } from '$lib/api/client';
	import {
		ACTIONS,
		INPUT_LABEL,
		MAX_PRECHECK,
		OUTCOME_LABEL,
		OUTCOME_WORD,
		POINTS,
		POINT_LABEL,
		TYPES,
		byPoint,
		choiceOptions,
		createGatesApi,
		criteriaFor,
		newQuestion,
		parseDetails,
		parseTools,
		playgroundState,
		ruleSummary,
		scoreLevels,
		toInput,
		validateQuestion,
		setThreshold,
		skippedKeys,
		type FireWhen,
		type GateInput,
		type GateEvaluation,
		type GatePoint,
		type GateQuestion,
		type GateType,
		type GateVerdict,
		type GatesView,
	} from '$lib/api/gates';
	import type { Paged, ToolInfo } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import AnswerView from './AnswerView.svelte';
	import CriteriaEditor from './CriteriaEditor.svelte';

	const gatesApi = createGatesApi({
		baseUrl: readConfig(env).apiUrl,
		token: getToken,
		job: () => app.job,
		activeTenant: () => app.activeTenant,
	});

	const gates = new Resource<GatesView>(
		() => gatesApi.list(),
		() => false,
	);
	gates.refresh();
	const verdicts = new Resource<GateVerdict[]>(() => gatesApi.verdicts(50));
	verdicts.refresh();
	// Tool names to pick from (the tool point's question list); a free list still works when this fails.
	const toolList = new Resource<Paged<ToolInfo>>(() => api().tools());
	toolList.refresh();
	const knownTools = $derived(
		toolList.value.state === 'ok' ? toolList.value.data.items.map((t) => t.name) : [],
	);

	const view = $derived(
		gates.value.state === 'ok' || gates.value.state === 'empty' ? gates.value.data : null,
	);
	const questions = $derived(view?.questions ?? []);
	const grouped = $derived(byPoint(questions));
	const canWrite = $derived(session.can('tenant_admin'));
	const denied = 'Needs the tenant_admin role';

	const FLOW: { before: string; point: GatePoint; after: string; hint: string }[] = [
		{
			before: 'Customer message',
			point: 'request',
			after: 'bus → workers',
			hint: 'Before the qualified request is published. Handoff/block replies with the message and stops.',
		},
		{
			before: 'worker step',
			point: 'tool',
			after: 'tools',
			hint: 'Before a listed tool is called. Fired: no call; the case goes to a human.',
		},
		{
			before: 'reply',
			point: 'response',
			after: 'customer',
			hint: 'After the reply is composed, before it is sent. Fired: the reply is replaced by the message.',
		},
	];

	// ---------- editor ----------
	let open = $state(false);
	let creating = $state(false);
	let draft = $state<GateQuestion | null>(null);
	let initial = '';
	/** Bumped per opened question: remounts the criteria editor, which reads its rows once. */
	let editSeq = $state(0);
	let toolsText = $state('');
	let critInvalid = $state('');
	let tried = $state(false);
	let saving = $state(false);
	let confirmDelete = $state(false);
	let serverError = $state('');
	const errors = $derived(
		draft
			? validateQuestion(
					draft,
					creating,
					questions.map((q) => q.key),
				)
			: {},
	);
	const blocked = $derived(critInvalid !== '' || Object.keys(errors).length > 0);
	const dirty = $derived(draft ? JSON.stringify(draft) !== initial : false);
	const levels = $derived(draft?.type === 'score' ? scoreLevels(draft.criteria) : []);
	const options = $derived(draft?.type === 'choice' ? choiceOptions(draft.criteria).map(([k]) => k) : []);

	function edit(q: GateQuestion, isNew = false) {
		creating = isNew;
		editSeq++;
		draft = structuredClone($state.snapshot(q)) as GateQuestion;
		initial = JSON.stringify(draft);
		toolsText = (q.tools ?? []).join(', ');
		critInvalid = '';
		tried = false;
		saving = false;
		confirmDelete = false;
		serverError = '';
		open = true;
	}
	function add(point: GatePoint) {
		const max = Math.max(-1, ...grouped[point].map((q) => q.ordinal));
		edit(newQuestion(point, max + 1), true);
	}
	function setType(t: GateType) {
		if (!draft) return;
		draft.criteria = criteriaFor(t, $state.snapshot(draft.criteria) as GateQuestion['criteria']);
		draft.type = t;
		critInvalid = '';
		if (t !== 'choice' && draft.fire_when === 'in') draft.fire_when = 'gte';
		if (t !== 'choice') draft.fire_values = null;
		if (t === 'score') {
			const n = scoreLevels(draft.criteria).length;
			if (draft.threshold == null || !Number.isInteger(draft.threshold) || draft.threshold >= n)
				draft.threshold = n - 1;
		} else if (draft.threshold == null || draft.threshold > 1) draft.threshold = 0.7;
	}
	/** Switching to `in` starts with no minimum confidence; leaving it needs a threshold again. */
	function setFireWhen(f: FireWhen) {
		if (!draft || f === draft.fire_when) return;
		if (f === 'in') draft.threshold = null;
		else if (draft.fire_when === 'in') {
			const t = setThreshold(draft.threshold);
			draft.threshold = t != null && t <= 1 ? t : 0.7;
		}
		draft.fire_when = f;
	}
	function setPoint(p: GatePoint) {
		if (!draft) return;
		draft.point = p;
		draft.tools = p === 'tool' ? parseTools(toolsText) : null;
		if (p !== 'request') draft.input = 'request';
	}
	function toggleFireValue(v: string, on: boolean) {
		if (!draft) return;
		const cur = new Set(draft.fire_values ?? []);
		if (on) cur.add(v);
		else cur.delete(v);
		draft.fire_values = [...cur];
	}
	function addTool(name: string) {
		const t = parseTools(toolsText);
		if (!t.includes(name)) t.push(name);
		toolsText = t.join(', ');
		if (draft) draft.tools = t;
	}
	async function save() {
		tried = true;
		if (!draft || blocked || saving || !canWrite) return;
		saving = true;
		serverError = '';
		const key = draft.key;
		const r = await guardOutcome(
			() => gatesApi.save(key, toInput($state.snapshot(draft) as GateQuestion)),
			creating ? `Added the ${key} gate.` : `Saved the ${key} gate.`,
		);
		saving = false;
		if (r.kind === 'ok') {
			open = false;
			await gates.refresh();
		} else if (r.kind === 'rejected') {
			if (r.error instanceof ApiError && r.error.status !== 403)
				serverError = `${r.error.message} (${r.error.code})`;
		} else {
			// No answer came back: re-read so the list shows what was stored; the edits stay in the drawer.
			await gates.refresh();
		}
	}
	async function remove() {
		if (!draft || saving || !canWrite) return;
		saving = true;
		const key = draft.key;
		const ok = await guard(() => gatesApi.remove(key), `Deleted the ${key} gate.`, 'Not deleted');
		saving = false;
		if (ok) {
			open = false;
			await gates.refresh();
		}
	}
	async function toggle(q: GateQuestion) {
		if (!canWrite) return;
		const next = { ...$state.snapshot(q), enabled: !q.enabled } as GateQuestion;
		await guard(
			() => gatesApi.save(q.key, toInput(next)),
			next.enabled ? `Enabled ${q.key}.` : `Disabled ${q.key}.`,
		);
		await gates.refresh();
	}

	// ---------- playground ----------
	let pgPoint = $state<string>('request');
	let pgText = $state('');
	let pgTool = $state('');
	let pgType = $state('');
	let pgDetails = $state('');
	let pgBusy = $state(false);
	let pgResult = $state<{ point: GatePoint; r: GateEvaluation } | null>(null);
	let pgError = $state('');
	const pgQuestions = $derived(
		pgResult
			? grouped[pgResult.point].filter(
					(q) =>
						q.enabled ||
						(pgResult!.r.answers && q.key in pgResult!.r.answers) ||
						pgResult!.r.fired_keys.includes(q.key),
				)
			: [],
	);
	async function evaluate() {
		const point = pgPoint as GatePoint;
		if (!canWrite || !pgText.trim() || pgBusy) return;
		const details = parseDetails(pgDetails);
		if (point === 'request' && pgType.trim() && typeof details === 'string') {
			pgError = details;
			return;
		}
		pgBusy = true;
		pgError = '';
		try {
			const request = pgType.trim() && typeof details !== 'string' ? { type: pgType, details } : undefined;
			const r = await gatesApi.evaluate(playgroundState(point, pgText, pgTool, request));
			pgResult = { point, r: { ...r, fired_keys: r.fired_keys ?? [] } };
			void verdicts.refresh();
		} catch (e) {
			pgResult = null;
			pgError = e instanceof ApiError ? `${e.message} (${e.code})` : (e as Error).message;
		} finally {
			pgBusy = false;
		}
	}
	const time = (s: string) => {
		const d = new Date(s);
		return Number.isNaN(d.getTime()) ? s : d.toLocaleString('en-GB');
	};
</script>

<header class="page-head">
	<div>
		<h1>Laya gates</h1>
		<p>
			System-1 questions laya answers at three points of a turn. A question that fires hands the customer to a
			human, blocks the step, or annotates the request.
		</p>
	</div>
	<div class="actions"><Freshness resource={gates} /></div>
</header>

{#if gates.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if gates.value.state === 'unavailable'}
	<Unavailable reason={gates.value.reason} retry={gates.retry} />
{:else if view}
	{#if view.provider.bound}
		<p class="provider" data-testid="gates-provider">
			<Status word="ok" label="laya bound" />
			<span class="muted">model</span>
			<span class="mono">{view.provider.model ?? 'default'}</span>
			<a href="/admin/providers">Providers</a>
		</p>
	{:else}
		<Callout kind="gap" title="Gates are off">
			No laya provider is bound to <span class="mono">gates</span>, so every gate point is skipped. Bind one
			on
			<a href="/admin/providers">Providers</a>.
		</Callout>
	{/if}

	<section class="flow" aria-label="Gate points">
		{#each FLOW as step (step.point)}
			<div class="node">{step.before}</div>
			<div class="arrow" aria-hidden="true">↓</div>
			<Card cls="flush">
				<section class="point" aria-labelledby="pt-{step.point}" data-testid="point-{step.point}">
					<div class="point-head">
						<div>
							<h2 id="pt-{step.point}">{POINT_LABEL[step.point]} gates</h2>
							<p class="muted hint">{step.hint}</p>
						</div>
						<button
							class="btn sm"
							type="button"
							disabled={!canWrite}
							title={canWrite ? '' : denied}
							onclick={() => add(step.point)}>Add {step.point} question</button
						>
					</div>
					{#if grouped[step.point].length === 0}
						<p class="empty-row muted">No questions at this point.</p>
					{:else}
						<div class="tbl-wrap">
							<table class="tbl">
								<thead
									><tr><th>Key</th><th>Type</th><th>Instructions</th><th>Rule</th><th>Enabled</th></tr></thead
								>
								<tbody>
									{#each grouped[step.point] as q (q.key)}
										<tr class:off={!q.enabled}>
											<td class="mono"
												><button type="button" class="table-action" onclick={() => edit(q)}>{q.key}</button
												></td
											>
											<td><Chip text={q.type} variant="accent" /></td>
											<td class="instr">{q.instructions}</td>
											<td class="rule"
												>{ruleSummary(q)}{#if q.point === 'tool' && q.tools?.length}<div class="muted mono">
														tools: {q.tools.join(', ')}
													</div>{/if}</td
											>
											<td
												><label class="toggle"
													><input
														type="checkbox"
														checked={q.enabled}
														disabled={!canWrite}
														title={canWrite ? '' : denied}
														aria-label="Enabled {q.key}"
														onchange={() => toggle(q)}
													/>{q.enabled ? 'On' : 'Off'}</label
												></td
											>
										</tr>
									{/each}
								</tbody>
							</table>
						</div>
					{/if}
				</section>
			</Card>
			<div class="arrow" aria-hidden="true">↓</div>
			<div class="node">{step.after}</div>
			{#if step.point !== 'response'}<div class="arrow" aria-hidden="true">⋮</div>{/if}
		{/each}
	</section>
{/if}

<Card title="Playground" sub="Asks laya the questions of one point. Each run is logged as a verdict.">
	{#if !canWrite}
		<Callout kind="info" title="Admins only"
			>Running the playground calls laya on the tenant's token, so it needs the tenant_admin role.</Callout
		>
	{/if}
	<div class="pg">
		<Seg
			items={POINTS.map((p) => [p, POINT_LABEL[p]] as [string, string])}
			bind:active={pgPoint}
			label="Point"
		/>
		<label class="field"
			><span class="label"
				>{pgPoint === 'response'
					? 'Reply text'
					: pgPoint === 'tool'
						? 'Customer request'
						: 'Customer message'}</span
			><textarea class="input" rows="3" bind:value={pgText}></textarea></label
		>
		{#if pgPoint === 'request'}
			<div class="form-grid">
				<label class="field"
					><span class="label">Request type (optional)</span><input
						class="input mono"
						bind:value={pgType}
						placeholder="delivery"
					/><span class="muted hint">Set it to see the request text of a turn with a draft.</span></label
				>
				<label class="field"
					><span class="label">Details (JSON, optional)</span><input
						class="input mono"
						bind:value={pgDetails}
						disabled={!pgType.trim()}
						placeholder={'{"order_id": "ORD-1042"}'}
					/></label
				>
			</div>
		{/if}
		{#if pgPoint === 'tool'}
			<div class="form-grid">
				<label class="field"
					><span class="label">Tool</span><input
						class="input mono"
						bind:value={pgTool}
						list="gate-tools"
						placeholder="order_issue_refund"
					/></label
				>
			</div>
		{/if}
		<div class="actions">
			<button
				class="btn primary"
				type="button"
				disabled={!canWrite || pgBusy || !pgText.trim()}
				title={canWrite ? '' : denied}
				onclick={evaluate}>{pgBusy ? 'Evaluating…' : 'Evaluate'}</button
			>
		</div>
		{#if pgError}<Callout kind="err" title="Not evaluated">{pgError}</Callout>{/if}
		{#if pgResult}
			{@const r = pgResult.r}
			<div class="result" data-testid="pg-result">
				<p class="summary">
					<Status word={OUTCOME_WORD[r.outcome] ?? 'idle'} label={OUTCOME_LABEL[r.outcome] ?? r.outcome} />
					{#if r.action}<Chip text={r.action} variant="err" />{/if}
					<span class="muted">latency {fmt.ms(r.latency_ms)}</span>
				</p>
				{#if r.message}<p>Message: <q>{r.message}</q></p>{/if}
				{#if r.error}<p class="err">{r.error}</p>{/if}
				{#if r.outcome === 'skipped'}
					<p class="muted">Gates are off: bind a laya provider on Providers.</p>
				{/if}
				{#each pgQuestions as q (q.key)}
					<AnswerView
						{q}
						a={r.answers?.[q.key] ?? null}
						fired={r.fired_keys.includes(q.key)}
						text={r.texts?.[q.key] ?? null}
					/>
				{/each}
			</div>
		{/if}
	</div>
</Card>

<Card cls="flush" title="Verdict log" sub="The latest 50 verdicts, newest first.">
	{#snippet actions()}<Freshness resource={verdicts} />{/snippet}
	{#if verdicts.value.state === 'loading'}
		<p class="muted pad">Loading…</p>
	{:else if verdicts.value.state === 'unavailable'}
		<Unavailable reason={verdicts.value.reason} retry={verdicts.retry} />
	{:else if verdicts.value.state === 'empty'}
		<div class="empty">No verdicts yet.</div>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl dense" data-testid="verdicts">
				<thead
					><tr
						><th>Time</th><th>Point</th><th>Outcome</th><th>Fired</th><th>Action</th><th class="r">Latency</th
						><th>Conversation / case</th><th>Error</th></tr
					></thead
				>
				<tbody>
					{#each verdicts.value.data as v (v.id)}
						<tr>
							<td class="nowrap">{time(v.created_at)}</td>
							<td>{v.point}</td>
							<td
								><Status
									word={OUTCOME_WORD[v.outcome] ?? 'idle'}
									label={OUTCOME_LABEL[v.outcome] ?? v.outcome}
								/></td
							>
							<td class="mono"
								>{v.fired_keys?.length ? v.fired_keys.join(', ') : '—'}{#if skippedKeys(v.answers).length}<div
										class="muted"
										data-testid="verdict-skipped"
									>
										skipped (precheck): {skippedKeys(v.answers).join(', ')}
									</div>{/if}</td
							>
							<td>{v.action ?? '—'}</td>
							<td class="r">{fmt.ms(v.latency_ms)}</td>
							<td class="mono"
								>{#if v.conversation_id || v.case_id}{v.conversation_id ?? '—'}{#if v.case_id}
										/ {v.case_id}{/if}{:else}<span class="muted">playground</span>{/if}</td
							>
							<td class="err-cell">{v.error ?? ''}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</Card>

<datalist id="gate-tools">
	{#each knownTools as t}<option value={t}></option>{/each}
</datalist>

<Drawer bind:open title={draft ? (creating ? 'New gate question' : draft.key) : ''} wide {dirty}>
	{#if draft}
		{#if !canWrite}
			<Callout kind="info" title="Read only"
				>You can view this question. Editing needs the tenant_admin role.</Callout
			>
		{/if}
		<fieldset class="gate-fields" disabled={!canWrite || saving}>
			<div class="form-grid">
				<label class="field"
					><span class="label">Key</span><input
						class="input mono"
						bind:value={draft.key}
						readonly={!creating}
						placeholder="emergency"
						aria-invalid={tried && errors.key ? 'true' : undefined}
					/>{#if tried && errors.key}<span class="field-err">{errors.key}</span>{/if}</label
				>
				<label class="field"
					><span class="label">Point</span>
					<select
						class="input"
						value={draft.point}
						onchange={(e) => setPoint((e.currentTarget as HTMLSelectElement).value as GatePoint)}
						>{#each POINTS as p}<option value={p}>{POINT_LABEL[p]}</option>{/each}</select
					></label
				>
				<label class="field"
					><span class="label">Type</span>
					<select
						class="input"
						value={draft.type}
						onchange={(e) => setType((e.currentTarget as HTMLSelectElement).value as GateType)}
						>{#each TYPES as t}<option value={t}
								>{t === 'noul'
									? 'noul (yes/no)'
									: t === 'choice'
										? 'choice (options)'
										: 'score (levels)'}</option
							>{/each}</select
					></label
				>
				<label class="field"
					><span class="label">Order</span><input
						class="input"
						type="number"
						bind:value={draft.ordinal}
					/></label
				>
			</div>
			<label class="field"
				><span class="label">Instructions</span><textarea
					class="input"
					rows="3"
					bind:value={draft.instructions}
					aria-invalid={tried && errors.instructions ? 'true' : undefined}
				></textarea>{#if tried && errors.instructions}<span class="field-err">{errors.instructions}</span
					>{/if}</label
			>
			{#key `${editSeq}|${draft.type}`}
				<CriteriaEditor
					type={draft.type}
					bind:criteria={draft.criteria}
					bind:invalid={critInvalid}
					disabled={!canWrite || saving}
					error={tried ? (errors.criteria ?? '') : ''}
				/>
			{/key}
			<div class="form-grid">
				<label class="field"
					><span class="label">Fires when</span>
					<select
						class="input"
						aria-label="Fires when"
						value={draft.fire_when}
						onchange={(e) => setFireWhen((e.currentTarget as HTMLSelectElement).value as FireWhen)}
					>
						<option value="gte">{draft.type === 'choice' ? 'confidence' : draft.type} ≥ threshold</option>
						<option value="lt">{draft.type === 'choice' ? 'confidence' : draft.type} &lt; threshold</option>
						{#if draft.type === 'choice'}<option value="in">choice is one of…</option>{/if}
					</select></label
				>
				{#if draft.fire_when === 'in'}
					<label class="field"
						><span class="label">Minimum confidence</span>
						<input
							class="input"
							aria-label="Minimum confidence"
							type="number"
							min="0"
							max="1"
							step="0.05"
							placeholder="any"
							bind:value={draft.threshold}
						/>
						{#if tried && errors.threshold}<span class="field-err">{errors.threshold}</span>{:else}<span
								class="muted hint">Blank: fires on the option alone.</span
							>{/if}</label
					>
				{:else}
					<label class="field"
						><span class="label">Threshold</span>
						{#if draft.type === 'score'}
							<select class="input" aria-label="Threshold" bind:value={draft.threshold}
								>{#each levels as l, i}<option value={i}>{i} · {l || '(unnamed)'}</option>{/each}</select
							>
						{:else}
							<input
								class="input"
								aria-label="Threshold"
								type="number"
								min="0"
								max="1"
								step="0.05"
								bind:value={draft.threshold}
							/>
						{/if}
						{#if tried && errors.threshold}<span class="field-err">{errors.threshold}</span>{/if}</label
					>
				{/if}
				<label class="field"
					><span class="label">Action</span>
					<select class="input" bind:value={draft.action}
						>{#each ACTIONS as a}<option value={a}>{a}</option>{/each}</select
					></label
				>
				<label class="field"
					><span class="label">When laya fails</span>
					<select class="input" bind:value={draft.fail}
						><option value="closed">fail closed (handoff)</option><option value="open"
							>fail open (pass)</option
						></select
					></label
				>
			</div>
			{#if draft.fire_when === 'in'}
				<fieldset class="opts">
					<legend class="label">Fires on</legend>
					{#each options as o}
						<label class="toggle"
							><input
								type="checkbox"
								checked={draft.fire_values?.includes(o) ?? false}
								onchange={(e) => toggleFireValue(o, (e.currentTarget as HTMLInputElement).checked)}
							/><span class="mono">{o}</span></label
						>
					{/each}
					{#if tried && errors.fire_values}<span class="field-err">{errors.fire_values}</span>{/if}
				</fieldset>
			{/if}
			<div class="form-grid">
				<label class="field"
					><span class="label">Asks laya about</span>
					<select
						class="input"
						aria-label="Asks laya about"
						value={draft.input ?? 'request'}
						disabled={draft.point !== 'request'}
						onchange={(e) => (draft!.input = (e.currentTarget as HTMLSelectElement).value as GateInput)}
						>{#each Object.entries(INPUT_LABEL) as [v, l]}<option value={v}>{l}</option>{/each}</select
					>
					{#if tried && errors.input}<span class="field-err">{errors.input}</span
						>{:else if draft.point !== 'request'}<span class="muted hint"
							>Tool and response questions read the full state.</span
						>{/if}</label
				>
				<label class="field"
					><span class="label">Precheck (optional)</span><input
						class="input mono"
						aria-label="Precheck"
						maxlength={MAX_PRECHECK}
						placeholder={'(?i)\\d{4,}|\\bpassword'}
						value={draft.precheck ?? ''}
						oninput={(e) => (draft!.precheck = (e.currentTarget as HTMLInputElement).value)}
						aria-invalid={tried && errors.precheck ? 'true' : undefined}
					/>{#if tried && errors.precheck}<span class="field-err">{errors.precheck}</span>{:else}<span
							class="muted hint"
							>A regular expression (Rust syntax; <span class="mono">(?i)</span> = ignore case). Laya is asked only
							when it matches the text above; otherwise the question is skipped and does not fire. Blank: always
							asked.</span
						>{/if}</label
				>
			</div>
			<label class="field"
				><span class="label">Message</span><textarea
					class="input"
					rows="2"
					placeholder="What the customer is told when this fires (blank: the default)"
					value={draft.message ?? ''}
					oninput={(e) => (draft!.message = (e.currentTarget as HTMLTextAreaElement).value)}
				></textarea></label
			>
			{#if draft.point === 'tool'}
				<label class="field"
					><span class="label">Tools</span><input
						class="input mono"
						bind:value={toolsText}
						oninput={() => (draft!.tools = parseTools(toolsText))}
						placeholder="order_issue_refund, order_create_rma"
						list="gate-tools"
					/>{#if tried && errors.tools}<span class="field-err">{errors.tools}</span>{/if}</label
				>
				{#if knownTools.length}
					<div class="chips">
						{#each knownTools.filter((t) => !(draft?.tools ?? []).includes(t)) as t}
							<button class="btn sm" type="button" onclick={() => addTool(t)}>+ {t}</button>
						{/each}
					</div>
				{/if}
			{/if}
			<label class="toggle"><input type="checkbox" bind:checked={draft.enabled} /> Enabled</label>
			<p class="muted">{ruleSummary(draft)}</p>
		</fieldset>
		{#if serverError}<Callout kind="err" title="Not saved">{serverError}</Callout>{/if}
		{#if tried && blocked}<p class="field-err">Fix the marked fields before saving.</p>{/if}
		{#if confirmDelete}
			<div class="actions">
				<span>Delete {draft.key}? Running conversations stop asking it.</span>
				<button class="btn" type="button" onclick={() => (confirmDelete = false)}>Keep</button>
				<button class="btn danger" type="button" disabled={saving} onclick={remove}>Delete</button>
			</div>
		{/if}
		<div class="actions">
			{#if !creating}
				<button
					class="btn danger"
					type="button"
					disabled={!canWrite || saving}
					title={canWrite ? '' : denied}
					onclick={() => (confirmDelete = true)}>Delete…</button
				>
			{/if}
			<button
				class="btn primary"
				type="button"
				disabled={!canWrite || saving}
				title={canWrite ? '' : denied}
				onclick={save}>{saving ? 'Saving…' : 'Save'}</button
			>
		</div>
	{/if}
</Drawer>

<style>
	.provider {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
		margin: 0 0 12px;
	}
	.flow {
		display: grid;
		gap: 6px;
		margin: 12px 0 20px;
	}
	.node {
		justify-self: center;
		font: 500 12.5px/1 var(--mono);
		padding: 6px 12px;
		border-radius: 999px;
		background: var(--sunk);
		border: 1px solid var(--line);
	}
	.arrow {
		justify-self: center;
		color: var(--faint);
		line-height: 1;
	}
	.point-head {
		display: flex;
		justify-content: space-between;
		gap: 12px;
		align-items: flex-start;
		padding: 12px 14px 6px;
		flex-wrap: wrap;
	}
	.point-head h2 {
		margin: 0;
		font-size: 15px;
	}
	.hint {
		margin: 2px 0 0;
		font-size: 12.5px;
	}
	.empty-row {
		padding: 4px 14px 12px;
		margin: 0;
	}
	.instr {
		max-width: 36ch;
	}
	.rule {
		font-size: 12.5px;
	}
	tr.off td {
		opacity: 0.6;
	}
	.toggle {
		display: inline-flex;
		gap: 6px;
		align-items: center;
	}
	.pg {
		display: grid;
		gap: 10px;
	}
	.result {
		display: grid;
		gap: 8px;
	}
	.summary {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
		margin: 0;
	}
	.err,
	.err-cell {
		color: var(--err);
	}
	.nowrap {
		white-space: nowrap;
	}
	.pad {
		padding: 12px 14px;
	}
	.gate-fields {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
	}
	.opts {
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		padding: 8px 12px;
		display: flex;
		gap: 12px;
		flex-wrap: wrap;
	}
	.field-err {
		color: var(--err);
		font-size: 12px;
	}
</style>
