<script lang="ts">
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		CHECKING_TEXT,
		classifyWriteError,
		compareProvider,
		compareRemoved,
		reconcile,
		reconcileText,
		type Comparison,
		type Reconciliation,
	} from '$lib/write-outcome';
	import ReconcileStatus from '$lib/ui/ReconcileStatus.svelte';
	import {
		PROBE_CAVEAT,
		TEMPERATURE_RANGE,
		draftFor,
		errorField,
		keyRequired,
		ruleFor,
		sourceLabel,
		toProviderInput,
		validateDraft,
		type ProviderCapability,
		type ProviderDraft,
		type ProviderView,
		type ProviderSlots,
	} from '$lib/api/providers';
	import Card from '$lib/ui/Card.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import LlmProviders from './LlmProviders.svelte';

	const providers = new Resource<ProviderSlots>(
		() => api().providerSlots(),
		(d) => d.capabilities.length === 0,
	);
	providers.refresh();
	const data = $derived(providers.value.state === 'ok' ? providers.value.data : null);
	const canWrite = $derived(session.can('tenant_admin'));

	let open = $state(false);
	let cap = $state<ProviderCapability | null>(null);
	let draft = $state<ProviderDraft | null>(null);
	let busy = $state(false);
	let baseline = $state('');
	const dirty = $derived(!!draft && JSON.stringify(draft) !== baseline);
	let touched = $state(false);
	/** The owner's refusal: shown next to its field when it names one, else above the actions. */
	let failure = $state<{ code: string; message: string } | null>(null);
	let confirmRemove = $state(false);
	/** The re-read after a write whose answer was lost, and how to judge it (Check again reuses it). */
	let check = $state<Reconciliation | null>(null);
	let pending: { capability: string; compare: (v: ProviderView | null) => Comparison } | null = null;

	const rule = $derived(cap ? ruleFor(cap.capability, cap.owner) : null);
	const errors = $derived(cap && draft ? validateDraft(cap, draft) : {});
	const failField = $derived(failure ? errorField(failure.code) : null);
	const fieldMsg = (f: keyof ProviderDraft) =>
		failField === f ? failure!.message : touched ? (errors[f] ?? '') : '';
	/** aria wiring for a field with a message, so it is announced with the input. */
	const faultAttrs = (f: keyof ProviderDraft) =>
		fieldMsg(f) ? { 'aria-invalid': 'true' as const, 'aria-describedby': `pv-err-${f}` } : {};
	const labelOf = (c: ProviderCapability) => ruleFor(c.capability, c.owner).label;

	function edit(c: ProviderCapability) {
		if (!c.available) return;
		cap = c;
		draft = draftFor(c);
		baseline = JSON.stringify(draft);
		touched = false;
		failure = null;
		confirmRemove = false;
		check = null;
		pending = null;
		open = true;
	}

	function apply(next: ProviderSlots) {
		providers.value = { state: 'ok', data: next };
		const fresh = next.capabilities.find((c) => c.capability === cap?.capability);
		if (fresh) cap = fresh;
	}

	async function run(
		action: () => Promise<ProviderSlots>,
		okMsg: string,
		compare: (v: ProviderView | null) => Comparison,
	) {
		if (busy || !canWrite || !cap) return;
		busy = true;
		failure = null;
		check = null;
		const capability = cap.capability;
		try {
			apply(await action());
			toast(okMsg);
			open = false;
		} catch (e) {
			if (classifyWriteError(e) === 'uncertain') {
				// No answer came back: re-read and compare instead of guessing. Never resubmits.
				toast(CHECKING_TEXT, 'err');
				pending = { capability, compare };
				busy = false;
				await verify();
				return;
			}
			if (e instanceof ApiError && e.status === 403)
				failure = { code: 'forbidden', message: 'Needs the tenant_admin role.' };
			else if (e instanceof ApiError) failure = { code: e.code, message: e.message };
			else failure = { code: 'error', message: (e as Error).message };
		} finally {
			busy = false;
		}
	}

	/** After a lost answer: re-read the providers and compare this capability with what was sent. */
	async function verify() {
		const p = pending;
		if (!p || busy) return;
		busy = true;
		check = { kind: 'checking' };
		const fresh: { v: ProviderSlots | null } = { v: null };
		const res = await reconcile(async () => {
			fresh.v = await api().providerSlots();
			const c = fresh.v.capabilities.find((x) => x.capability === p.capability);
			// An owner that cannot be asked proves nothing either way.
			if (!c?.available) throw new Error('owner unavailable');
			return c.source === 'tenant' ? c.provider : null;
		}, p.compare);
		busy = false;
		if (pending !== p) return;
		check = res;
		if (fresh.v) apply(fresh.v);
		if (res.kind === 'confirmed') {
			// The drawer closes, so the result goes to the toast.
			check = null;
			toast(reconcileText(res));
			pending = null;
			open = false;
		}
	}

	function save() {
		if (!cap || !draft) return;
		touched = true;
		if (Object.keys(errors).length > 0) return;
		const c = cap;
		const input = toProviderInput(c, draft);
		run(
			() => api().saveProviderSlot(c.capability, input),
			`Registered the ${labelOf(c)} provider.`,
			(v) => compareProvider(input, v),
		);
	}

	function remove() {
		if (!cap) return;
		const c = cap;
		run(() => api().removeProviderSlot(c.capability), `Removed the ${labelOf(c)} provider.`, compareRemoved);
	}
</script>

<header class="page-head">
	<div>
		<h1>Providers</h1>
		<p>
			The tenant's own model providers: the LLMs care-bus calls itself (request qualification and Laya gates),
			and the knowledge and ingestion capabilities served by strata_rag and strata_ingest. Tokens and keys are
			write-only: once saved they are never shown again.
		</p>
	</div>
</header>

<LlmProviders />

<header class="page-head">
	<div>
		<h2>Knowledge and ingestion</h2>
		<p>
			Each capability is served by the service that uses it: strata_rag for embeddings, reranking and chat,
			strata_ingest for OCR, vision and transcription. Without a registration the deployment default applies.
		</p>
	</div>
</header>
<Freshness resource={providers} />

{#if providers.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if providers.value.state === 'unavailable'}
	<Unavailable reason={providers.value.reason} retry={() => providers.refresh()} />
{:else if providers.value.state === 'empty'}
	<div class="empty">No provider capabilities are reported.</div>
{:else if data}
	{#each data.services.filter((s) => s.status !== 'ok') as s}
		<Callout
			kind="err"
			title={`${s.name} ${s.status === 'unconfigured' ? 'is not configured' : 'is unavailable'}`}
			>Its providers cannot be read or changed right now. This is not the same as no provider being
			registered.</Callout
		>
	{/each}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead>
					<tr
						><th>Capability</th><th>Service</th><th>Source</th><th>Model</th><th>Endpoint</th><th>Key</th><th
							>Updated</th
						></tr
					>
				</thead>
				<tbody>
					{#each data.capabilities as c (c.capability)}
						{@const st = sourceLabel(c)}
						<tr
							><td><button class="table-action" onclick={() => edit(c)}>{labelOf(c)}</button></td>
							<td class="mono">{c.owner}</td>
							<td><Status word={st.word} label={st.label} /></td>
							<td class="mono">
								{#if c.provider}{c.provider.dialect} · {c.provider.model}{#if c.provider.dims != null}
										· {c.provider.dims}d{/if}
								{:else if c.default}{c.default.dialect} · {c.default.model}{#if c.default.dims != null}
										· {c.default.dims}d{/if}
								{:else}—{/if}
							</td>
							<td class="mono">{c.provider?.base_url ?? '—'}</td>
							<td>{c.provider ? (c.provider.has_api_key ? 'Stored' : 'None') : '—'}</td>
							<td>{c.provider?.updated_at ? c.provider.updated_at.slice(0, 16).replace('T', ' ') : '—'}</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
	<p class="muted">{PROBE_CAVEAT}</p>
{/if}

<Drawer bind:open title={cap ? labelOf(cap) : ''} wide {dirty}>
	{#if cap && draft && rule}
		<p class="muted">{rule.purpose}</p>
		{#if cap.source === 'default' && cap.default}
			<Callout kind="info" title="Using the deployment default"
				>{cap.default.dialect} · {cap.default.model}. Registering a provider replaces it for this tenant.</Callout
			>
		{/if}
		<Callout kind="info" title="Before saving">{PROBE_CAVEAT}</Callout>
		{#if !canWrite}
			<Callout kind="info" title="Read only">Changing providers needs the tenant_admin role.</Callout>
		{/if}
		<fieldset class="pv-fields" disabled={!canWrite || busy}>
			<div class="form-grid">
				<label class="field"
					><span class="label">Base URL</span><input
						class="input mono"
						bind:value={draft.base_url}
						{...faultAttrs('base_url')}
						placeholder="https://api.example.eu/v1"
						autocomplete="off"
						spellcheck="false"
					/>{#if fieldMsg('base_url')}<span class="field-err" id="pv-err-base_url"
							>{fieldMsg('base_url')}</span
						>{/if}</label
				>
				<label class="field"
					><span class="label">Dialect</span>
					<select class="input" bind:value={draft.dialect} {...faultAttrs('dialect')}
						>{#each rule.dialects as d}<option>{d}</option>{/each}</select
					>{#if fieldMsg('dialect')}<span class="field-err" id="pv-err-dialect">{fieldMsg('dialect')}</span
						>{/if}
				</label>
				<label class="field"
					><span class="label">Model</span><input
						class="input mono"
						bind:value={draft.model}
						{...faultAttrs('model')}
					/>{#if fieldMsg('model')}<span class="field-err" id="pv-err-model">{fieldMsg('model')}</span
						>{/if}</label
				>
				<label class="field"
					><span class="label">Model id (optional)</span><input
						class="input mono"
						bind:value={draft.model_id}
						placeholder="stored and shown only"
					/></label
				>
				{#if rule.dims !== null}
					<label class="field"
						><span class="label">Dimensions</span><input
							class="input"
							type="number"
							bind:value={draft.dims}
							{...faultAttrs('dims')}
						/>{#if fieldMsg('dims')}<span class="field-err" id="pv-err-dims">{fieldMsg('dims')}</span
							>{/if}</label
					>
				{/if}
				<label class="field"
					><span class="label">Regions</span><input
						class="input"
						bind:value={draft.regions}
						{...faultAttrs('regions')}
						placeholder="eu"
					/>{#if fieldMsg('regions')}<span class="field-err" id="pv-err-regions">{fieldMsg('regions')}</span
						>{/if}</label
				>
				<label class="field"
					><span class="label">Timeout (ms, at most {rule.maxTimeoutMs})</span><input
						class="input"
						type="number"
						min="1"
						max={rule.maxTimeoutMs}
						bind:value={draft.timeout_ms}
						{...faultAttrs('timeout_ms')}
					/>{#if fieldMsg('timeout_ms')}<span class="field-err" id="pv-err-timeout_ms"
							>{fieldMsg('timeout_ms')}</span
						>{/if}</label
				>
				<label class="field"
					><span class="label">Concurrency (1 to {rule.maxConcurrency})</span><input
						class="input"
						type="number"
						min="1"
						max={rule.maxConcurrency}
						bind:value={draft.concurrency}
						{...faultAttrs('concurrency')}
					/>{#if fieldMsg('concurrency')}<span class="field-err" id="pv-err-concurrency"
							>{fieldMsg('concurrency')}</span
						>{/if}</label
				>
				{#if rule.chatSettings}
					<label class="field"
						><span class="label">Thinking</span>
						<span
							><input
								type="checkbox"
								checked={draft.thinking ?? false}
								onchange={(e) => draft && (draft.thinking = e.currentTarget.checked)}
							/>
							{draft.thinking === null
								? 'Not sent (provider default)'
								: draft.thinking
									? 'On: the model reasons before answering'
									: 'Off'}</span
						>
					</label>
					<label class="field"
						><span class="label"
							>Temperature (optional, {TEMPERATURE_RANGE.min} to {TEMPERATURE_RANGE.max})</span
						><input
							class="input"
							type="number"
							step="0.1"
							min={TEMPERATURE_RANGE.min}
							max={TEMPERATURE_RANGE.max}
							bind:value={draft.temperature}
							{...faultAttrs('temperature')}
							placeholder="provider default"
						/>{#if fieldMsg('temperature')}<span class="field-err" id="pv-err-temperature"
								>{fieldMsg('temperature')}</span
							>{/if}</label
					>
				{/if}
				<label class="field"
					><span class="label">Zero data retention</span>
					<span
						><input type="checkbox" bind:checked={draft.zdr} {...faultAttrs('zdr')} /> The provider keeps no request
						data</span
					>{#if fieldMsg('zdr')}<span class="field-err" id="pv-err-zdr">{fieldMsg('zdr')}</span>{/if}
				</label>
				<label class="field"
					><span class="label">API key</span><input
						class="input mono"
						type="password"
						bind:value={draft.api_key}
						{...faultAttrs('api_key')}
						placeholder={keyRequired(cap, draft) ? 'required' : 'stored; leave blank to keep it'}
						autocomplete="new-password"
						spellcheck="false"
					/>{#if fieldMsg('api_key')}<span class="field-err" id="pv-err-api_key">{fieldMsg('api_key')}</span
						>{/if}</label
				>
			</div>
		</fieldset>
		{#if failure && !failField}
			<Callout kind="err" title="Not saved">{failure.message} ({failure.code})</Callout>
		{/if}
		<ReconcileStatus result={check} retry={verify} />
		{#if confirmRemove}
			<div class="callout err" role="alertdialog" aria-label="Remove provider">
				<strong>Remove the {labelOf(cap)} provider?</strong>
				<span
					>{cap.owner} forgets the registration and its key. {cap.default
						? 'The deployment default applies again.'
						: 'Nothing serves this capability until a provider is registered.'}</span
				>
				<div class="actions">
					<button class="btn" type="button" onclick={() => (confirmRemove = false)}>Keep</button>
					<button class="btn danger" type="button" disabled={busy} onclick={remove}>Remove</button>
				</div>
			</div>
		{/if}
		<div class="actions">
			{#if cap.source === 'tenant'}
				<button
					class="btn"
					type="button"
					disabled={!canWrite || busy}
					title={canWrite ? '' : 'Needs the tenant_admin role'}
					onclick={() => (confirmRemove = true)}>Remove registration</button
				>
			{/if}
			<button
				class="btn primary"
				type="button"
				disabled={!canWrite || busy}
				title={canWrite ? '' : 'Needs the tenant_admin role'}
				onclick={save}>{busy ? 'Testing and saving…' : 'Save'}</button
			>
		</div>
	{/if}
</Drawer>

<style>
	.pv-fields {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
	}
	.field-err {
		color: var(--err);
		font-size: 12px;
	}
</style>
