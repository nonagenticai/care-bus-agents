<script lang="ts">
	// LLM providers (spec 2026-10-18 §2): the tenant's OpenAI-compatible and Laya endpoints, and which one serves
	// each use (USE_RULES: qualify, gates, research, research_write). Tokens are write-only: a stored token is never shown, only whether one is set.
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { classifyWriteError } from '$lib/write-outcome';
	import { RESEARCH_ENABLED, SHOWN_LLM_USES } from '$lib/features';
	import {
		KIND_LABELS,
		LLM_KINDS,
		LLM_USES,
		USE_RULES,
		baseUrlProblem,
		bindingOf,
		draftTestProblem,
		formModelChoices,
		llmDraftFor,
		modelChoices,
		providersFor,
		testLine,
		toLlmInput,
		toTestInput,
		unboundNotice,
		useForInitial,
		usesForKind,
		useForPlan,
		useSummary,
		usesOf,
		validateLlmDraft,
		type LlmDraft,
		type UseForChoice,
		type LlmProvider,
		type LlmProviderTest,
		type LlmProviders,
		type LlmUse,
	} from '$lib/api/providers';
	import Card from '$lib/ui/Card.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const ROLE_HINT = 'Needs the tenant_admin role';

	const llm = new Resource<LlmProviders>(
		() => api().llmProviders(),
		() => false,
	);
	llm.refresh();
	const data = $derived(llm.value.state === 'ok' ? llm.value.data : null);
	const canWrite = $derived(session.can('tenant_admin'));

	/** The last Test of each provider, by id: feeds the model pickers below. */
	let tests = $state<Record<string, LlmProviderTest | { error: string }>>({});
	let testing = $state<string | null>(null);
	const lastTest = (id: string | null): LlmProviderTest | undefined => {
		const t = id ? tests[id] : undefined;
		return t && 'ok' in t ? t : undefined;
	};

	/** Turns any write error into a line for the user; a lost answer is said to be unknown, never "failed". */
	function errText(e: unknown): string {
		if (classifyWriteError(e) === 'uncertain')
			return 'No answer came back, so it may or may not have been saved. The list was reloaded; check it before trying again.';
		if (e instanceof ApiError && e.status === 403) return `${ROLE_HINT}.`;
		if (e instanceof ApiError) return `${e.message} (${e.code})`;
		return (e as Error)?.message ?? String(e);
	}

	async function test(p: LlmProvider) {
		if (!canWrite || testing) return;
		testing = p.id;
		try {
			tests[p.id] = await api().testLlmProvider(p.id);
		} catch (e) {
			tests[p.id] = { error: errText(e) };
		} finally {
			testing = null;
		}
	}

	// ---- add / edit drawer ------------------------------------------------------------------------------------

	let open = $state(false);
	let editing = $state<LlmProvider | null>(null);
	let draft = $state<LlmDraft>(llmDraftFor(null));
	let baseline = $state('');
	let touched = $state(false);
	let busy = $state(false);
	let failure = $state<string | null>(null);
	let confirmDelete = $state(false);
	/** "Use for" per use; only the uses matching the form's kind are shown and applied on Save. */
	let useFor = $state<Record<LlmUse, UseForChoice>>(useForInitial(null, null));
	const formUses = $derived(usesForKind(draft.kind).filter((u) => SHOWN_LLM_USES.includes(u)));
	const dirty = $derived(open && JSON.stringify({ draft, useFor }) !== baseline);
	const errors = $derived(validateLlmDraft(draft));
	const fieldMsg = (f: 'name' | 'kind' | 'base_url') => (touched ? (errors[f] ?? '') : '');
	const faultAttrs = (f: 'name' | 'kind' | 'base_url') =>
		fieldMsg(f) ? { 'aria-invalid': 'true' as const, 'aria-describedby': `llm-err-${f}` } : {};

	/** The in-form Test: the form's values, unsaved. */
	let formTest = $state<LlmProviderTest | { error: string } | null>(null);
	let formTesting = $state(false);
	const formTested = $derived(formTest && 'ok' in formTest ? formTest : undefined);
	const formModels = (u: LlmUse) =>
		formModelChoices(draft.kind, formTested ?? lastTest(editing?.id ?? null), useFor[u].model || null);

	async function testForm() {
		if (!canWrite || formTesting) return;
		const urlProblem = baseUrlProblem(draft.base_url);
		if (urlProblem) {
			formTest = { error: urlProblem };
			return;
		}
		formTesting = true;
		try {
			formTest = await api().testLlmProviderDraft(toTestInput(draft, editing?.id ?? null));
		} catch (e) {
			const known = e instanceof ApiError ? draftTestProblem(e.code) : null;
			formTest = { error: known ?? errText(e) };
		} finally {
			formTesting = false;
		}
	}

	function edit(p: LlmProvider | null, opts: { kind?: LlmProvider['kind']; bindUse?: LlmUse } = {}) {
		editing = p;
		draft = llmDraftFor(p);
		if (!p && opts.kind) draft.kind = opts.kind;
		useFor = useForInitial(data, p?.id ?? null);
		baseline = JSON.stringify({ draft, useFor });
		if (opts.bindUse) useFor[opts.bindUse].on = true;
		formTest = null;
		touched = false;
		failure = null;
		confirmDelete = false;
		open = true;
	}

	async function write(action: () => Promise<unknown>, okMsg: string) {
		if (busy || !canWrite) return;
		busy = true;
		failure = null;
		try {
			await action();
			toast(okMsg);
			open = false;
		} catch (e) {
			failure =
				e instanceof ApiError && e.code === 'provider_in_use'
					? 'This provider is still bound to a use. Bind that use to another provider (or none) under Uses first.'
					: errText(e);
		} finally {
			busy = false;
			await llm.refresh();
		}
	}

	/** Saves the provider, then applies "Use for". A failed binding keeps the drawer open on the saved provider. */
	async function save() {
		touched = true;
		if (Object.keys(errors).length > 0 || busy || !canWrite) return;
		const input = toLlmInput(draft);
		const p = editing;
		const uses = [...formUses];
		const choices = $state.snapshot(useFor);
		const before = data;
		busy = true;
		failure = null;
		let saved: LlmProvider;
		try {
			saved = p ? await api().updateLlmProvider(p.id, input) : await api().createLlmProvider(input);
		} catch (e) {
			failure =
				e instanceof ApiError && e.code === 'provider_in_use'
					? 'This provider is still bound to a use of another kind. Bind that use to another provider (or none) under Uses first.'
					: errText(e);
			busy = false;
			await llm.refresh();
			return;
		}
		// Each use of the form's kind in turn; the first failure stops the rest and keeps the drawer open.
		const done: string[] = [];
		let current: LlmUse | null = null;
		let plan: ReturnType<typeof useForPlan> = null;
		try {
			for (const use of uses) {
				current = use;
				plan = useForPlan(before, p?.id ?? null, use, choices[use], saved.id);
				if (!plan) continue;
				await api().bindLlmUse(use, plan);
				done.push(
					plan.provider_id ? ` ${USE_RULES[use].label} now uses it.` : ` ${USE_RULES[use].label} unbound.`,
				);
			}
			toast(`${p ? 'Saved' : 'Added'} ${input.name}.${done.join('')}`);
			open = false;
		} catch (e) {
			// The provider is stored: further saves update it instead of adding a second one.
			editing = saved;
			draft.token = '';
			draft.clearToken = false;
			const label = current ? USE_RULES[current].label : '';
			failure = `${input.name} was saved, but ${label} was not ${plan?.provider_id ? 'bound' : 'unbound'}: ${errText(e)}`;
		} finally {
			busy = false;
			await llm.refresh();
		}
	}

	function remove() {
		const p = editing;
		if (!p) return;
		void write(() => api().deleteLlmProvider(p.id), `Deleted ${p.name}.`);
	}

	// ---- uses -------------------------------------------------------------------------------------------------

	/** The uses form: what is selected, per use, until saved. Reset from the server after every read. */
	const perUse = <V,>(v: () => V) => Object.fromEntries(LLM_USES.map((u) => [u, v()])) as Record<LlmUse, V>;
	let picks = $state<Record<LlmUse, { provider_id: string; model: string }>>(
		perUse(() => ({ provider_id: '', model: '' })),
	);
	let usesBusy = $state<LlmUse | null>(null);
	let useFailure = $state<Record<LlmUse, string>>(perUse(() => ''));
	$effect(() => {
		if (!data) return;
		for (const u of LLM_USES) {
			const b = bindingOf(data, u);
			picks[u] = { provider_id: b.provider_id ?? '', model: b.model ?? '' };
		}
	});
	const useDirty = (d: LlmProviders, u: LlmUse) => {
		const b = bindingOf(d, u);
		return (b.provider_id ?? '') !== picks[u].provider_id || (b.model ?? '') !== picks[u].model.trim();
	};

	/**
	 * The "Bind" shortcut on an unbound use: pick the first provider of its kind in the use's own form (the model is
	 * then chosen and saved there), or, with none yet, open Add provider with that kind and "Use for" checked.
	 */
	function bindShortcut(d: LlmProviders, u: LlmUse) {
		if (!canWrite) return;
		const first = providersFor(d, u)[0];
		if (!first) {
			edit(null, { kind: USE_RULES[u].kind, bindUse: u });
			return;
		}
		picks[u].provider_id = first.id;
		document.getElementById(`llm-use-${u}-provider`)?.focus();
	}

	// ---- external LLMs (research runs only) ----------------------------------------------------------------------

	let externalBusy = $state(false);
	let externalFailure = $state('');

	async function setExternal(on: boolean) {
		if (!canWrite || externalBusy) return;
		externalBusy = true;
		externalFailure = '';
		try {
			await api().setLlmSettings({ external_llm_allowed: on });
			toast(
				on ? 'Research runs may now use external LLMs.' : 'Research runs now use only the platform model.',
			);
		} catch (e) {
			externalFailure = errText(e);
		} finally {
			externalBusy = false;
			await llm.refresh();
		}
	}

	async function bind(u: LlmUse) {
		if (!canWrite || usesBusy) return;
		usesBusy = u;
		useFailure[u] = '';
		const pid = picks[u].provider_id || null;
		const model = pid ? picks[u].model.trim() || null : null;
		try {
			await api().bindLlmUse(u, { provider_id: pid, model });
			toast(pid ? `${USE_RULES[u].label} now uses the chosen provider.` : `${USE_RULES[u].label} unbound.`);
		} catch (e) {
			useFailure[u] = errText(e);
		} finally {
			usesBusy = null;
			await llm.refresh();
		}
	}
</script>

{#if llm.value.state === 'loading'}
	<p class="muted">Loading LLM providers…</p>
{:else if llm.value.state === 'unavailable'}
	<Unavailable reason={llm.value.reason} retry={llm.retry} />
{:else if data}
	<Card
		title="LLM providers"
		sub="OpenAI-compatible endpoints and Laya. Tokens are write-only: once saved they are never shown again."
		cls="flush"
	>
		{#snippet actions()}
			<button
				class="btn primary sm"
				type="button"
				disabled={!canWrite}
				title={canWrite ? '' : ROLE_HINT}
				onclick={() => edit(null)}>Add provider</button
			>
		{/snippet}
		{#if data.providers.length === 0}
			<div class="empty">No LLM providers yet.</div>
		{:else}
			<div class="tbl-wrap">
				<table class="tbl" aria-label="LLM providers">
					<thead>
						<tr><th>Name</th><th>Kind</th><th>Base URL</th><th>Token</th><th>Updated</th><th>Test</th></tr>
					</thead>
					<tbody>
						{#each data.providers as p (p.id)}
							{@const t = tests[p.id]}
							<tr
								><td><button class="table-action" onclick={() => edit(p)}>{p.name}</button></td>
								<td>{KIND_LABELS[p.kind] ?? p.kind}</td>
								<td class="mono">{p.base_url}</td>
								<td>{p.has_token ? 'Set' : 'Not set'}</td>
								<td
									>{p.updated_at.slice(0, 16).replace('T', ' ')}{#if p.updated_by}
										· {p.updated_by}{/if}</td
								>
								<td>
									<button
										class="btn sm"
										type="button"
										aria-label={`Test ${p.name}`}
										disabled={!canWrite || testing !== null}
										title={canWrite ? '' : ROLE_HINT}
										onclick={() => test(p)}>{testing === p.id ? 'Testing…' : 'Test'}</button
									>
									{#if t}
										<div class="llm-test" role="status" aria-label={`Test result for ${p.name}`}>
											{#if 'ok' in t}
												<Status word={t.ok ? 'ok' : 'err'} label={testLine(t)} />
												{#if t.models.length}<span class="muted mono">{t.models.join(', ')}</span>{/if}
											{:else}
												<Status word="err" label={t.error} />
											{/if}
										</div>
									{/if}
								</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{/if}
	</Card>

	<Card title="Uses" sub="Which provider serves each use. A use only takes providers of its kind.">
		{#if !canWrite}
			<Callout kind="info" title="Read only">Changing providers needs the tenant_admin role.</Callout>
		{/if}
		{#each SHOWN_LLM_USES as u (u)}
			{@const rule = USE_RULES[u]}
			{@const sum = useSummary(data, u)}
			{@const choices = providersFor(data, u)}
			{@const notice = unboundNotice(data, u)}
			{@const models = modelChoices(lastTest(picks[u].provider_id || null), picks[u].model || null)}
			<section class="llm-use" aria-label={`Use ${rule.label}`}>
				<div class="llm-use-head">
					<strong>{rule.label}</strong>
					<span class="mono muted">{u}</span>
					<Status word={sum.word} label={sum.text} />
				</div>
				<p class="muted">{rule.purpose}</p>
				{#if notice}
					<div class="callout gap llm-unbound" role="status" aria-label={`${u} not bound`}>
						<strong class="mono">{notice}</strong>
						<button
							class="btn sm"
							type="button"
							disabled={!canWrite}
							title={canWrite ? '' : ROLE_HINT}
							onclick={() => bindShortcut(data, u)}>Bind {u}</button
						>
					</div>
				{/if}
				<fieldset class="llm-fields" disabled={!canWrite || usesBusy !== null}>
					<div class="form-grid">
						<label class="field"
							><span class="label">{rule.label} provider</span>
							<select class="input" id={`llm-use-${u}-provider`} bind:value={picks[u].provider_id}>
								<option value="">{rule.none}</option>
								{#each choices as p (p.id)}<option value={p.id}>{p.name}</option>{/each}
							</select>
							{#if choices.length === 0}<span class="muted"
									>Add a {rule.kind === 'laya' ? 'Laya' : 'OpenAI-compatible'} provider first.</span
								>{/if}
						</label>
						<label class="field"
							><span class="label">{rule.label} model</span>
							{#if models}
								<select class="input mono" bind:value={picks[u].model} disabled={!picks[u].provider_id}>
									<option value="">Choose a model</option>
									{#each models as m (m)}<option value={m}>{m}</option>{/each}
								</select>
							{:else}
								<input
									class="input mono"
									bind:value={picks[u].model}
									disabled={!picks[u].provider_id}
									placeholder="Test the provider to pick from its models"
									autocomplete="off"
									spellcheck="false"
								/>
							{/if}
						</label>
					</div>
				</fieldset>
				{#if useFailure[u]}
					<Callout kind="err" title="Not saved">{useFailure[u]}</Callout>
				{/if}
				<div class="actions">
					<button
						class="btn"
						type="button"
						disabled={!canWrite || usesBusy !== null || !useDirty(data, u)}
						title={canWrite ? '' : ROLE_HINT}
						onclick={() => bind(u)}>{usesBusy === u ? 'Saving…' : `Save ${rule.label}`}</button
					>
				</div>
			</section>
		{/each}
	</Card>

	{#if RESEARCH_ENABLED}
		<Card
			title="External LLMs"
			sub="Research runs only. Chat qualification and Laya gates are not affected by this setting."
		>
			<label class="toggle"
				><input
					type="checkbox"
					aria-label="Allow external LLMs for research runs"
					checked={data.external_llm_allowed}
					disabled={!canWrite || externalBusy}
					title={canWrite ? '' : ROLE_HINT}
					onchange={(e) => setExternal(e.currentTarget.checked)}
				/> Allow research runs to use a provider other than the platform model</label
			>
			<p class="muted">
				Off by default. While off, a research run whose <span class="mono">research</span> or
				<span class="mono">research_write</span> provider is not the platform model fails with
				<span class="mono">external_llm_not_allowed</span>; it never falls back to another model.
			</p>
			{#if externalFailure}
				<Callout kind="err" title="Not saved">{externalFailure}</Callout>
			{/if}
		</Card>
	{/if}
{/if}

<Drawer bind:open title={editing ? editing.name : 'Add LLM provider'} {dirty}>
	{#if !canWrite}
		<Callout kind="info" title="Read only">Changing providers needs the tenant_admin role.</Callout>
	{/if}
	<fieldset class="llm-fields" disabled={!canWrite || busy}>
		<div class="form-grid">
			<label class="field"
				><span class="label">Name</span><input
					class="input"
					bind:value={draft.name}
					{...faultAttrs('name')}
					autocomplete="off"
				/>{#if fieldMsg('name')}<span class="field-err" id="llm-err-name">{fieldMsg('name')}</span
					>{/if}</label
			>
			<label class="field"
				><span class="label">Kind</span>
				<select class="input" bind:value={draft.kind} {...faultAttrs('kind')}
					>{#each LLM_KINDS as k (k)}<option value={k}>{KIND_LABELS[k]}</option>{/each}</select
				>{#if fieldMsg('kind')}<span class="field-err" id="llm-err-kind">{fieldMsg('kind')}</span>{/if}
			</label>
			<label class="field"
				><span class="label">Base URL</span><input
					class="input mono"
					bind:value={draft.base_url}
					{...faultAttrs('base_url')}
					placeholder={draft.kind === 'laya' ? 'https://laya.example.eu' : 'https://llm.example.eu/v1'}
					autocomplete="off"
					spellcheck="false"
				/>{#if fieldMsg('base_url')}<span class="field-err" id="llm-err-base_url">{fieldMsg('base_url')}</span
					>{/if}</label
			>
			<label class="field"
				><span class="label">Token</span><input
					class="input mono"
					type="password"
					bind:value={draft.token}
					disabled={draft.clearToken}
					placeholder={editing?.has_token
						? 'stored; leave blank to keep it'
						: draft.kind === 'laya'
							? 'optional; blank uses the tenant licence'
							: 'optional'}
					autocomplete="new-password"
					spellcheck="false"
				/>
				{#if editing?.has_token}
					<span class="row">
						{#if draft.clearToken}
							<span class="muted">The stored token is removed on save.</span>
							<button class="btn sm" type="button" onclick={() => (draft.clearToken = false)}
								>Keep token</button
							>
						{:else}
							<button
								class="btn sm"
								type="button"
								onclick={() => {
									draft.clearToken = true;
									draft.token = '';
								}}>Clear token</button
							>
						{/if}
					</span>
				{/if}
			</label>
		</div>
		<div class="llm-form-test">
			<button
				class="btn sm"
				type="button"
				disabled={!canWrite || formTesting}
				title={canWrite ? 'Tests the values in this form; nothing is saved' : ROLE_HINT}
				onclick={testForm}>{formTesting ? 'Testing…' : 'Test'}</button
			>
			<span class="muted">Tests the URL and token above without saving.</span>
			{#if formTest}
				<div class="llm-test" role="status" aria-label="Form test result">
					{#if 'ok' in formTest}
						<Status word={formTest.ok ? 'ok' : 'err'} label={testLine(formTest)} />
						{#if formTest.models.length}<span class="muted mono">{formTest.models.join(', ')}</span
							>{:else}<span class="muted">No models listed.</span>{/if}
					{:else}
						<Status word="err" label={formTest.error} />
					{/if}
				</div>
			{/if}
		</div>
		<fieldset class="llm-use-for">
			<legend class="label">Use for</legend>
			{#each formUses as u (u)}
				{@const fm = formModels(u)}
				<label class="toggle"
					><input type="checkbox" bind:checked={useFor[u].on} /> {USE_RULES[u].useFor}</label
				>
				{#if useFor[u].on}
					<label class="field"
						><span class="label">{USE_RULES[u].label} model</span>
						{#if fm.choices}
							<select class="input mono" bind:value={useFor[u].model}>
								<option value="">Choose a model</option>
								{#each fm.choices as m (m)}<option value={m}>{m}</option>{/each}
							</select>
						{:else}
							<input
								class="input mono"
								bind:value={useFor[u].model}
								list={fm.suggestions.length ? `llm-model-suggestions-${u}` : undefined}
								placeholder={fm.suggestions.length
									? 'jev-latest, or Test to list the models'
									: 'Test to list the models'}
								autocomplete="off"
								spellcheck="false"
							/>
							<datalist id={`llm-model-suggestions-${u}`}>
								{#each fm.suggestions as m (m)}<option value={m}></option>{/each}
							</datalist>
						{/if}
					</label>
					<span class="muted">Saved with the provider: {USE_RULES[u].purpose}</span>
				{:else if data && editing && bindingOf(data, u).provider_id === editing.id}
					<span class="muted">Saving unbinds {u} from this provider.</span>
				{/if}
			{/each}
		</fieldset>
	</fieldset>
	{#if editing && data}
		<p class="muted">
			Used by: {usesOf(data, editing.id)
				.map((u) => USE_RULES[u].label)
				.join(', ') || 'nothing'}
		</p>
	{/if}
	{#if failure}
		<Callout kind="err" title="Not saved">{failure}</Callout>
	{/if}
	{#if confirmDelete && editing}
		<div class="callout err" role="alertdialog" aria-label="Delete provider">
			<strong>Delete {editing.name}?</strong>
			<span>Its token is deleted with it.</span>
			<div class="actions">
				<button class="btn" type="button" onclick={() => (confirmDelete = false)}>Keep</button>
				<button class="btn danger" type="button" disabled={busy} onclick={remove}>Delete</button>
			</div>
		</div>
	{/if}
	<div class="actions">
		{#if editing}
			<button
				class="btn"
				type="button"
				disabled={!canWrite || busy}
				title={canWrite ? '' : ROLE_HINT}
				onclick={() => (confirmDelete = true)}>Delete provider</button
			>
		{/if}
		<button
			class="btn primary"
			type="button"
			disabled={!canWrite || busy}
			title={canWrite ? '' : ROLE_HINT}
			onclick={save}>{busy ? 'Saving…' : 'Save'}</button
		>
	</div>
</Drawer>

<style>
	.llm-fields {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
	}
	.field-err {
		color: var(--err);
		font-size: 12px;
	}
	.llm-test {
		display: flex;
		flex-direction: column;
		gap: 2px;
		margin-top: 4px;
		font-size: 12px;
	}
	.llm-use + .llm-use {
		border-top: 1px solid var(--line);
		margin-top: 12px;
		padding-top: 12px;
	}
	.llm-form-test {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
		margin-top: 12px;
	}
	.llm-form-test .llm-test {
		flex-basis: 100%;
	}
	.llm-use-for {
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		padding: 8px 12px;
		margin: 12px 0 0;
		display: grid;
		gap: 8px;
	}
	.toggle {
		display: inline-flex;
		gap: 6px;
		align-items: center;
	}
	.llm-unbound {
		display: flex;
		gap: 8px;
		align-items: center;
		justify-content: space-between;
		flex-wrap: wrap;
		margin: 8px 0;
	}
	.llm-use-head {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
	}
</style>
