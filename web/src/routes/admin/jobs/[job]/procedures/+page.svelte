<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Latest, Resource } from '$lib/load.svelte';
	import { nextStatuses } from './transitions';
	import {
		LIFECYCLE,
		draftSteps,
		errorField,
		groupPhases,
		historyText,
		nextVersion,
		paramBindings,
		parseJson,
		statusFilters,
		stepInputs,
		stepSummary,
		successView,
		versionHistory,
		type FormField,
	} from './view';
	import { ApiError } from '$lib/api/client';
	import type {
		ContextSpace,
		Paged,
		ProcedureDetail,
		ProcedureInput,
		ProcedureSummary,
		ToolInfo,
		VersionStatus,
	} from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import Meter from '$lib/ui/Meter.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';

	const procs = new Resource<Paged<ProcedureSummary>>(() => api().procedures());
	procs.refresh();
	let open = $state(false);
	let sel = $state<ProcedureDetail | null>(null);
	let selError = $state('');
	let tab = $state('flow');
	let filter = $state('all');
	const canWrite = $derived(session.can('tenant_admin'));
	const all = $derived(procs.value.state === 'ok' ? procs.value.data.items : []);
	const rows = $derived(filter === 'all' ? all : all.filter((p) => p.version_status === filter));
	const history = $derived(sel ? versionHistory(all, sel.name) : []);
	const flow = $derived(sel ? groupPhases(sel.steps, sel.plan) : []);
	const day = (iso: string) => iso.slice(0, 10);
	const latestDetail = new Latest();
	async function show(p: Pick<ProcedureSummary, 'name' | 'version'>, keepTab = false) {
		const ticket = latestDetail.next();
		open = true;
		if (!keepTab) tab = 'flow';
		sel = null;
		selError = '';
		try {
			const d = await api().procedure(p.name, p.version);
			if (latestDetail.stale(ticket)) return;
			sel = d;
		} catch (e) {
			if (latestDetail.stale(ticket)) return;
			selError = (e as Error).message;
		}
	}
	// The PUBLISHED version of the selected procedure, when it is not the selected one (roll-back target).
	const current = $derived(
		sel
			? all.find(
					(p) => p.name === sel!.name && p.version_status === 'PUBLISHED' && p.version !== sel!.version,
				)
			: undefined,
	);
	let confirmRollback = $state(false);
	$effect(() => {
		void sel;
		confirmRollback = false;
	});
	// The caller's own approval of the selected version, so the button never offers an approval that changes nothing.
	const approvedByMe = $derived(!!sel && !!session.user?.actor && sel.reviewers.includes(session.user.actor));
	let approvalNote = $state('');
	// Keyed on a primitive: re-reading the same version after an approval replaces `sel` and must keep the note.
	const selKey = $derived(sel ? `${sel.name}@${sel.version}` : '');
	$effect(() => {
		void selKey;
		approvalNote = '';
	});
	// plan 05: PUBLISHED needs two distinct reviewers. Approving (POST .../approve, CANARY only) and publishing
	// (POST .../status) are separate calls; a publish with fewer than two reviewers is refused and changes nothing.
	const approvalsNeeded = $derived(sel ? Math.max(0, 2 - sel.reviewers.length) : 2);
	let approving = $state(false);
	function approve(p: ProcedureDetail) {
		if (approving || moving) return;
		approving = true;
		const got: { detail?: ProcedureDetail } = {};
		guard(
			async () => {
				got.detail = await api().approveProcedure(p.name, p.version);
			},
			`Approved ${p.name}@${p.version}.`,
			'Not approved',
		)
			.then(async (ok) => {
				if (!ok || !got.detail) return;
				// The response is the updated detail; apply it only if the drawer still shows this version.
				if (selKey === `${got.detail.name}@${got.detail.version}`) {
					sel = got.detail;
					approvalNote = 'Your approval was recorded.';
				}
				await procs.refresh();
			})
			.finally(() => (approving = false));
	}
	let moving = $state(false);
	function move(p: ProcedureDetail, to: VersionStatus) {
		if (moving || approving) return;
		moving = true;
		const rollback = p.version_status === 'ARCHIVED' && to === 'PUBLISHED';
		const okMsg = rollback
			? `Rolled ${p.name} back to ${p.version}.`
			: `${p.name}@${p.version} is now ${to}.`;
		guard(() => api().setProcedureStatus(p.name, p.version, to), okMsg)
			.then(async (ok) => {
				if (!ok) return;
				open = false;
				await procs.refresh();
			})
			.finally(() => (moving = false));
	}

	// New procedure version: POST /api/procedures. Plan and steps are JSON; the API is the validator and its 400s
	// are shown under the field they concern.
	const spaces = new Resource<Paged<ContextSpace>>(() => api().contextSpaces());
	const tools = new Resource<Paged<ToolInfo>>(() => api().tools());
	const spaceList = $derived(spaces.value.state === 'ok' ? spaces.value.data.items : []);
	const toolNames = $derived(tools.value.state === 'ok' ? tools.value.data.items.map((t) => t.name) : []);
	const EMPTY_PLAN =
		'{\n  "phases": [\n    { "steps": [ { "worker": "", "action": "", "as": "", "args": {} } ], "gate": null }\n  ]\n}';
	let creating = $state(false);
	let form = $state({
		from: '',
		name: '',
		version: '1.0.0',
		context_space: '',
		description: '',
		pddl_domain: '',
		plan: EMPTY_PLAN,
		steps: '[]',
	});
	let formErr = $state<{ field: FormField; code: string; message: string } | null>(null);
	let saving = $state(false);
	const planJson = $derived(parseJson(form.plan));
	const stepsJson = $derived(parseJson(form.steps));
	const formDirty = $derived(form.name !== '' || form.plan !== EMPTY_PLAN || form.steps !== '[]');
	function startNew() {
		form = {
			from: '',
			name: '',
			version: '1.0.0',
			context_space: '',
			description: '',
			pddl_domain: '',
			plan: EMPTY_PLAN,
			steps: '[]',
		};
		formErr = null;
		creating = true;
		if (spaces.value.state !== 'ok') spaces.refresh();
		if (tools.value.state !== 'ok') tools.refresh();
	}
	async function startFrom(key: string) {
		form.from = key;
		formErr = null;
		const p = all.find((x) => `${x.name}@${x.version}` === key);
		if (!p) return;
		try {
			const d = await api().procedure(p.name, p.version);
			form = {
				from: key,
				name: d.name,
				version: nextVersion(d.version),
				context_space: d.context_space,
				description: d.description ?? '',
				pddl_domain: d.pddl_domain ?? '',
				plan: JSON.stringify(d.plan, null, 2),
				steps: JSON.stringify(stepInputs(d.steps), null, 2),
			};
		} catch (e) {
			formErr = {
				field: null,
				code: 'load_failed',
				message: `Could not load ${key}: ${(e as Error).message}`,
			};
		}
	}
	function fillSteps() {
		if (!planJson.ok) return;
		if (formErr?.field === 'steps') formErr = null;
		form.steps = JSON.stringify(draftSteps(planJson.value, toolNames), null, 2);
	}
	async function submit(e: SubmitEvent) {
		e.preventDefault();
		formErr = null;
		if (!planJson.ok) return (formErr = { field: 'plan', code: 'invalid_json', message: planJson.error });
		if (!stepsJson.ok) return (formErr = { field: 'steps', code: 'invalid_json', message: stepsJson.error });
		const body: ProcedureInput = {
			name: form.name.trim(),
			version: form.version.trim(),
			context_space: form.context_space,
			description: form.description.trim() || null,
			pddl_domain: form.pddl_domain.trim() || null,
			plan: planJson.value,
			steps: stepsJson.value as ProcedureInput['steps'],
		};
		saving = true;
		try {
			const d = await api().createProcedure(body);
			creating = false;
			await procs.refresh();
			show(d);
		} catch (err) {
			if (err instanceof ApiError)
				formErr = {
					field: errorField(err.code),
					code: err.code,
					message: err.status === 403 ? 'You need the tenant_admin role to do this.' : err.message,
				};
			else formErr = { field: null, code: 'error', message: (err as Error).message };
		} finally {
			saving = false;
		}
	}
</script>

{#snippet fieldErr(f: FormField)}
	{#if formErr && formErr.field === f}
		<span class="pr-err" role="alert">{formErr.message} <span class="mono">({formErr.code})</span></span>
	{/if}
{/snippet}

<header class="page-head">
	<div>
		<h1>Procedures</h1>
		<p>
			What the orchestrator does for each context space. A procedure is a versioned list of steps; each step
			is a PDDL action with preconditions and effects, carried out by one worker through one MCP tool.
		</p>
	</div>
	<div class="actions">
		<button
			class="btn primary"
			type="button"
			disabled={!canWrite}
			title={canWrite ? 'Create a DRAFT version' : 'Needs the tenant_admin role'}
			onclick={startNew}>New procedure</button
		>
	</div>
</header>
<Freshness resource={procs} />

{#if procs.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if procs.value.state === 'unavailable'}
	<Unavailable reason={procs.value.reason} retry={() => procs.refresh()} />
{:else if procs.value.state === 'empty'}
	<div class="empty">No procedures yet.</div>
{:else}
	<div class="split pr-bar">
		<Seg items={statusFilters(all)} bind:active={filter} />
		<span class="muted pr-hint"
			>The orchestrator runs the PUBLISHED version of each context space. Runs and success count the last 7
			days of cases recorded against each version.</span
		>
	</div>
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead>
					<tr
						><th>Procedure</th><th>Version</th><th>Status</th><th>Context space</th><th class="r">Steps</th
						><th class="r" title="Resolved cases this version ran in the last 7 days">Runs 7d</th><th
							title="Completed or not eligible, out of the runs in the last 7 days; only failed counts as a failure"
							>Success</th
						></tr
					>
				</thead>
				<tbody>
					{#each rows as p (p.name + '@' + p.version)}
						{@const sum = stepSummary(p)}
						{@const ok = successView(p.success_rate)}
						<tr tabindex="0" onclick={() => show(p)} onkeydown={(e) => e.key === 'Enter' && show(p)}>
							<td
								><span class="cell-title mono">{p.name}</span><span class="cell-sub"
									>domain {p.domain} · updated {day(p.updated_at)} · {p.reviewers.length} reviewer{p.reviewers
										.length === 1
										? ''
										: 's'}</span
								></td
							>
							<td class="mono">{p.version}</td>
							<td><Status word={p.version_status} /></td>
							<td><Chip text={p.context_space} /></td>
							<td class="r"
								>{sum.steps}{#if sum.phases}
									<span class="muted">in {sum.phases} phase{sum.phases === 1 ? '' : 's'}</span>{/if}</td
							>
							<td class="r mono">{p.runs_7d}</td>
							<td class="pr-success"
								>{#if ok}<Meter value={p.success_rate ?? 0} cls={ok.cls} />
									<span class="mono">{ok.text}</span>{:else}<span class="muted">—</span>{/if}</td
							>
						</tr>
					{:else}
						<tr><td colspan="7" class="muted">No procedures with this status.</td></tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

<Drawer bind:open title={sel ? `${sel.name}@${sel.version}` : 'Procedure'} wide>
	{#snippet sub()}
		{#if sel}
			<Status word={sel.version_status} />
			<Chip text={sel.domain} />
			<span class="muted">updated {day(sel.updated_at)}</span>
		{/if}
	{/snippet}
	{#if selError}
		<Unavailable reason={selError} />
	{:else if !sel}
		<p class="muted">Loading…</p>
	{:else}
		<dl class="kvs pr-kvs">
			<dt>Context space</dt>
			<dd class="mono">{sel.context_space}</dd>
			{#if sel.description}
				<dt>Description</dt>
				<dd>{sel.description}</dd>
			{/if}
			<dt>Tool requirements</dt>
			<dd>
				<span class="chips"
					>{#each sel.tool_requirements as t}<Chip text={t} variant="tool" />{:else}<span class="muted"
							>none</span
						>{/each}</span
				>
			</dd>
			<dt>Success criteria</dt>
			<dd>
				<span class="chips"
					>{#each sel.success_criteria as c}<Chip text={c} variant="ok" />{:else}<span class="muted"
							>none</span
						>{/each}</span
				>
			</dd>
			<dt>API</dt>
			<dd class="mono">GET /api/procedures/{sel.name}/versions/{sel.version}</dd>
		</dl>
		<Tabs
			items={[
				['flow', 'Flow'],
				['steps', 'Steps', sel.steps.length],
				['pddl', 'PDDL'],
				['versions', 'Versions', history.length],
			]}
			bind:active={tab}
		/>
		{#if tab === 'flow'}
			{#if flow.length}
				<div class="pr-flow">
					{#each flow as ph, i (ph.phase)}
						{#if i}<div class="pr-arrow" aria-hidden="true">→</div>{/if}
						<div class="pr-phase">
							<span class="label">{ph.label}</span>
							{#each ph.steps as s (s.step_number)}
								<div class="pr-step">
									<span class="pr-n">{s.step_number}</span>
									<span class="mono pr-act">{s.action_type}</span>
									<span class="cell-sub">{s.performed_by} → {s.mcp_tool}</span>
									{#if s.postconditions.length}
										<span class="chips"
											>{#each s.postconditions as c}<Chip text={c} variant="ok" />{/each}</span
										>
									{/if}
								</div>
							{/each}
							{#if ph.gate}
								<div class="pr-gate">
									Gate on <span class="mono">{ph.gate}.eligible</span>. If false, the case ends as not
									eligible.
								</div>
							{/if}
						</div>
					{/each}
				</div>
				<p class="note">
					Steps in one phase run in parallel. A phase starts when every step of the previous one succeeded.
				</p>
			{:else}
				<div class="empty">This version has no steps.</div>
			{/if}
		{:else if tab === 'steps'}
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead>
						<tr
							><th class="r">#</th><th>action_type</th><th>Preconditions</th><th>Postconditions</th><th
								>Parameters</th
							><th>Role</th><th>Tool</th></tr
						>
					</thead>
					<tbody>
						{#each sel.steps as s (s.step_number)}
							<tr>
								<td class="r">{s.step_number}</td>
								<td class="mono"
									>{s.action_type}{#if s.gate}
										<Chip text="gate" variant="warn" />{/if}</td
								>
								<td
									><span class="chips"
										>{#each s.preconditions as c}<Chip text={c} variant="warn" />{:else}<span class="muted"
												>none</span
											>{/each}</span
									></td
								>
								<td
									><span class="chips"
										>{#each s.postconditions as c}<Chip text={c} variant="ok" />{/each}</span
									></td
								>
								<td class="mono pr-params"
									>{#each paramBindings(s, sel.plan) as [k, v]}<span>{k} ← {v}</span>{:else}<span
											class="muted">none</span
										>{/each}</td
								>
								<td>{s.performed_by}</td>
								<td><Chip text={s.mcp_tool} variant="tool" /></td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else if tab === 'pddl'}
			{#if sel.pddl_domain}
				<pre class="code">{sel.pddl_domain}</pre>
			{:else}
				<div class="empty">No PDDL domain stored for this version.</div>
			{/if}
		{:else}
			<div class="pr-versions">
				<div class="pr-states">
					{#each LIFECYCLE as s, i}
						{#if i}<span class="muted" aria-hidden="true">→</span>{/if}
						<span class="pr-state" class:on={s === sel.version_status}>{s}</span>
					{/each}
					{#if sel.version_status === 'DISCARDED'}
						<span class="pr-state on">DISCARDED</span>
					{/if}
				</div>
				<h3>Changes to {sel.version}</h3>
				{#if sel.history.length}
					<ol class="pr-hist">
						{#each sel.history as h, i (i)}
							<li class:pr-hist-bad={!h.ok}>
								<span class="mono pr-hist-at">{h.at.replace('T', ' ').slice(0, 16)}</span>
								<span>{historyText(h)}</span>
								<span class="muted">by {h.actor}</span>
							</li>
						{/each}
					</ol>
				{:else}
					<p class="muted">
						No recorded changes. This version was loaded by migration before the audit trail recorded it.
					</p>
				{/if}
				<Callout
					kind={sel.reviewers.length >= 2 ? 'info' : 'gap'}
					title="Reviewers: {sel.reviewers.length} of 2"
				>
					{sel.reviewers.length
						? `Approved by ${sel.reviewers.join(' and ')}.`
						: 'Nobody has reviewed this version yet.'} Two approvals are needed before a version can be PUBLISHED.
				</Callout>
				{#if approvalNote}<p class="note" role="status">{approvalNote}</p>{/if}
				{#if history.length}
					<div class="tbl-wrap">
						<table class="tbl dense clickable">
							<thead>
								<tr><th>Version</th><th>Status</th><th>Updated</th><th>Reviewers</th></tr>
							</thead>
							<tbody>
								{#each history as h (h.version)}
									<tr
										tabindex="0"
										class:pr-cur={h.version === sel.version}
										onclick={() => show(h, true)}
										onkeydown={(e) => e.key === 'Enter' && show(h, true)}
									>
										<td class="mono">{h.version}</td>
										<td><Status word={h.version_status} /></td>
										<td class="mono">{day(h.updated_at)}</td>
										<td>{h.reviewers.join(', ') || '—'}</td>
									</tr>
								{/each}
							</tbody>
						</table>
					</div>
				{/if}
				<div class="row">
					{#each nextStatuses(sel.version_status) as to}
						{#if to === 'PUBLISHED' && sel.version_status === 'ARCHIVED'}
							{#if confirmRollback}
								<div class="pr-confirm" role="group" aria-label="Confirm roll back">
									<span
										>Re-publish {sel.name}@{sel.version}?
										{current
											? `${current.version} (PUBLISHED now) moves to ARCHIVED.`
											: 'No version of this procedure is PUBLISHED now.'}
										{approvalsNeeded
											? `Publishing needs two reviewers; this version has ${sel.reviewers.length}.`
											: ''}</span
									>
									<button
										class="btn primary"
										type="button"
										disabled={moving || approvalsNeeded > 0}
										onclick={() => move(sel!, to)}>Roll back to {sel.version}</button
									>
									<button class="btn" type="button" onclick={() => (confirmRollback = false)}>Cancel</button>
								</div>
							{:else}
								<button
									class="btn"
									type="button"
									disabled={!canWrite}
									title={canWrite ? 'Re-publish this archived version' : 'Needs the tenant_admin role'}
									onclick={() => (confirmRollback = true)}>Roll back to this version</button
								>
							{/if}
						{:else if to === 'PUBLISHED'}
							<button
								class="btn"
								type="button"
								disabled={!canWrite || approving || moving || approvedByMe}
								title={!canWrite
									? 'Needs the tenant_admin role'
									: approvedByMe
										? 'You approved this version; a second, distinct reviewer approves next'
										: 'Adds you as a reviewer of this version; it does not publish'}
								onclick={() => approve(sel!)}>Approve</button
							>
							{#if approvedByMe}<span class="muted">You approved this version.</span>{/if}
							<button
								class="btn primary"
								type="button"
								disabled={!canWrite || moving || approving || approvalsNeeded > 0}
								title={!canWrite
									? 'Needs the tenant_admin role'
									: approvalsNeeded
										? 'Publishing needs two distinct reviewers'
										: 'Make this the version the orchestrator runs'}
								onclick={() => move(sel!, to)}>Publish</button
							>
							{#if approvalsNeeded}<span class="muted"
									>Needs {approvalsNeeded} more approval{approvalsNeeded === 1 ? '' : 's'}</span
								>{/if}
						{:else}
							<button
								class="btn"
								class:danger={to === 'DISCARDED'}
								type="button"
								disabled={!canWrite || moving}
								title={canWrite ? '' : 'Needs the tenant_admin role'}
								onclick={() => move(sel!, to)}>Move to {to}</button
							>
						{/if}
					{:else}
						<span class="muted">No further moves from {sel.version_status}.</span>
					{/each}
				</div>
				<p class="note">
					Approvals call POST /api/procedures/{sel.name}/versions/{sel.version}/approve; status changes call
					POST /api/procedures/{sel.name}/versions/{sel.version}/status.
				</p>
			</div>
		{/if}
	{/if}
</Drawer>

<Drawer bind:open={creating} title="New procedure version" wide dirty={formDirty}>
	<form class="pr-form" onsubmit={submit} novalidate>
		<p class="muted">
			Creates a DRAFT. Steps must describe the plan one-to-one: ordered by step number, each step has the
			phase and worker of the matching plan step. Promote it from the Versions tab.
		</p>
		<label class="field"
			><span class="label">Start from</span>
			<select class="input" value={form.from} onchange={(e) => startFrom(e.currentTarget.value)}>
				<option value="">Empty plan</option>
				{#each all as p (p.name + '@' + p.version)}
					<option value={`${p.name}@${p.version}`}>{p.name}@{p.version} ({p.version_status})</option>
				{/each}
			</select>
		</label>
		<div class="form-grid">
			<label class="field"
				><span class="label">Name</span><input
					class="input mono"
					bind:value={form.name}
					required
					placeholder="return"
				/>{@render fieldErr('name')}</label
			>
			<label class="field"
				><span class="label">Version</span><input
					class="input mono"
					bind:value={form.version}
					required
					placeholder="1.5.0"
				/>{@render fieldErr('version')}</label
			>
			<label class="field"
				><span class="label">Context space</span>
				{#if spaceList.length}
					<select class="input" bind:value={form.context_space}>
						<option value="" disabled>Choose a context space</option>
						{#each spaceList as sp (sp.key)}<option value={sp.key}>{sp.label} ({sp.key})</option>{/each}
					</select>
				{:else}
					<input
						class="input mono"
						bind:value={form.context_space}
						placeholder="retail:order-support:returns"
					/>
				{/if}
				{@render fieldErr('context_space')}</label
			>
			<label class="field"
				><span class="label">Description</span><input class="input" bind:value={form.description} /></label
			>
		</div>
		<label class="field"
			><span class="label">Plan (JSON: phases of steps the orchestrator runs)</span>
			<textarea class="input" rows="10" spellcheck="false" bind:value={form.plan}></textarea>
			{#if !planJson.ok}<span class="pr-err">{planJson.error}</span>{/if}
			{@render fieldErr('plan')}</label
		>
		<div class="field">
			<span class="split"
				><label class="label" for="pr-steps">Steps (JSON: one PDDL step per plan step)</label><button
					class="btn sm"
					type="button"
					disabled={!planJson.ok}
					onclick={fillSteps}>Draft steps from plan</button
				></span
			>
			<textarea id="pr-steps" class="input" rows="10" spellcheck="false" bind:value={form.steps}></textarea>
			{#if !stepsJson.ok}<span class="pr-err">{stepsJson.error}</span>{/if}
			{@render fieldErr('steps')}
		</div>
		<label class="field"
			><span class="label">PDDL domain (optional, stored as text)</span>
			<textarea class="input" rows="4" spellcheck="false" bind:value={form.pddl_domain}></textarea></label
		>
		{#if formErr && formErr.field === null}
			<Callout kind="err" title="Not created">{formErr.message} ({formErr.code})</Callout>
		{/if}
		<div class="row">
			<button class="btn primary" type="submit" disabled={saving || !canWrite}
				>{saving ? 'Creating…' : 'Create DRAFT'}</button
			>
			<button class="btn" type="button" onclick={() => (creating = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<style>
	.pr-form {
		display: grid;
		gap: 12px;
	}
	.pr-err {
		color: var(--err);
		font-size: 12px;
	}
	.pr-hist {
		list-style: none;
		margin: 0;
		padding: 0;
		display: grid;
		gap: 4px;
		font-size: 12.5px;
	}
	.pr-hist li {
		display: flex;
		flex-wrap: wrap;
		gap: 4px 10px;
		padding: 4px 0;
		border-bottom: 1px dashed var(--line);
	}
	.pr-hist-at {
		color: var(--faint);
	}
	.pr-hist-bad span:nth-child(2) {
		color: var(--warn);
	}
	.pr-confirm {
		display: flex;
		flex-wrap: wrap;
		gap: 8px;
		align-items: center;
		padding: 10px;
		border: 1px solid var(--warn);
		border-radius: 6px;
	}
	.pr-bar {
		margin-bottom: 12px;
	}
	.pr-success {
		white-space: nowrap;
	}
	.pr-hint {
		font-size: 12.5px;
	}
	.pr-kvs {
		margin-bottom: 14px;
	}
	.pr-flow {
		display: flex;
		gap: 10px;
		align-items: stretch;
		overflow-x: auto;
		padding-bottom: 4px;
		margin-top: 12px;
	}
	.pr-phase {
		display: grid;
		gap: 8px;
		align-content: start;
		min-width: 200px;
		flex: 1;
		padding: 10px;
		border: 1px dashed var(--line);
		border-radius: 8px;
	}
	.pr-arrow {
		align-self: center;
		color: var(--faint);
		font-size: 18px;
	}
	.pr-step {
		display: grid;
		gap: 4px;
		padding: 8px 10px 8px 34px;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--panel);
		position: relative;
	}
	.pr-n {
		position: absolute;
		left: 8px;
		top: 8px;
		width: 18px;
		height: 18px;
		border-radius: 50%;
		background: var(--sunk);
		font: 600 11px/18px var(--mono);
		text-align: center;
		color: var(--muted);
	}
	.pr-act {
		font-weight: 500;
	}
	.pr-gate {
		font-size: 12px;
		color: var(--warn);
		border-top: 1px dashed var(--warn);
		padding-top: 6px;
	}
	.pr-params {
		display: grid;
		gap: 2px;
		font-size: 12px;
	}
	.pr-versions {
		display: grid;
		gap: 12px;
		margin-top: 12px;
	}
	.pr-states {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
	.pr-state {
		font: 500 11.5px var(--mono);
		padding: 4px 8px;
		border-radius: 4px;
		background: var(--sunk);
		color: var(--muted);
	}
	.pr-state.on {
		background: var(--accent);
		color: var(--panel);
	}
	.pr-cur td {
		background: var(--sunk);
	}
	@media (max-width: 640px) {
		.pr-flow {
			flex-direction: column;
		}
		.pr-arrow {
			transform: rotate(90deg);
			align-self: center;
		}
	}
</style>
