<script lang="ts">
	import { tick } from 'svelte';
	import { api, guard, guardOutcome } from '$lib/app.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import { compareSource, orNull, reconcile, reconcileText, type Reconciliation } from '$lib/write-outcome';
	import ReconcileStatus from '$lib/ui/ReconcileStatus.svelte';
	import { looksLikeSecret } from '$lib/api/client';
	import {
		attributeOptions,
		deleteMessage,
		draftDirty,
		draftErrors,
		draftOf,
		inputOf,
		leafStores,
		SOURCE_TYPES,
		structured,
		testErrorText,
		testPlan,
		WEBSITE_MODES,
	} from '$lib/sources';
	import type {
		ApiSample,
		ColumnPreview,
		OntologyEntity,
		Source,
		SourceDeleted,
		SourceInput,
		StoreNode,
	} from '$lib/api/types';
	import Callout from '$lib/ui/Callout.svelte';
	import JsonView from '$lib/ui/JsonView.svelte';
	import MappingEditor from './MappingEditor.svelte';

	let {
		source,
		stores,
		entities,
		canWrite,
		onsaved,
		dirty = $bindable(false),
	}: {
		source: Source | null;
		stores: StoreNode[];
		entities: OntologyEntity[];
		canWrite: boolean;
		onsaved: (s: Source | null) => void;
		/** True while the form differs from the stored source (or from an empty form when adding). */
		dirty?: boolean;
	} = $props();

	let draft = $state(draftOf(null));
	let sample = $state<ApiSample | null>(null);
	let preview = $state<ColumnPreview | null>(null);
	let saving = $state(false);
	// The last previewed file, so ticking the header box re-reads it with the new setting.
	let previewed: File | null = null;
	let testing = $state(false);
	let testError = $state('');
	let blockedReasons = $state<string[]>([]);
	let confirmDelete = $state(false);
	let purge = $state(false);
	/** The re-read after a save whose answer was lost, and what that save sent (Check again compares with it). */
	let check = $state<Reconciliation | null>(null);
	let pending: { id: string | null; input: SourceInput } | null = null;
	$effect(() => {
		draft = draftOf(source);
		sample = null;
		preview = null;
		previewed = null;
		testError = '';
		blockedReasons = [];
		confirmDelete = false;
		purge = false;
		check = null;
		pending = null;
	});

	const errors = $derived(draftErrors(draft));
	$effect(() => {
		dirty = source ? draftDirty(draft, source) : JSON.stringify(draft) !== JSON.stringify(draftOf(null));
	});
	const uses = $derived(structured(draft.type) && draft.authType !== 'none');
	const secretInSettings = $derived([draft.url, draft.headerName, draft.sheet].some(looksLikeSecret));
	const knownFields = $derived(preview?.columns ?? sample?.fields ?? []);
	const why = $derived(!canWrite ? 'Needs the tenant_admin role' : '');
	const plan = $derived(testPlan(draft, source, { secretInSettings }));
	const storeOptions = $derived(leafStores(stores));
	// Clear a stale "what is missing" list as soon as the draft becomes testable.
	$effect(() => {
		if (plan.kind !== 'blocked') blockedReasons = [];
	});

	/** Saves the draft; resolves to the stored source (the drawer switches to it), or null when refused. */
	async function save(): Promise<Source | null> {
		if (saving || !canWrite || errors.length || secretInSettings) return null;
		saving = true;
		check = null;
		const box: { v: Source | null } = { v: null };
		const id = source?.id ?? null;
		const input = inputOf(draft, source?.has_credentials ?? false);
		const r = await guardOutcome(
			async () => {
				box.v = id ? await api().saveSource(id, input) : await api().createSource(input);
			},
			id ? 'Source saved.' : 'Source added.',
			'Not saved',
			{ reconcile: true },
		);
		saving = false;
		if (r.kind === 'ok' && box.v) {
			onsaved(box.v);
			// Let the parent hand the saved source back in (it resets the draft) before anything is shown for it.
			await tick();
			return box.v;
		}
		if (r.kind === 'uncertain') {
			pending = { id, input };
			return verify();
		}
		return null;
	}
	/**
	 * After a lost answer: re-read the source (a new one by its name, which is unique) and compare it with what
	 * was sent. Never resubmits.
	 */
	async function verify(): Promise<Source | null> {
		const p = pending;
		if (!p || saving) return null;
		saving = true;
		check = { kind: 'checking' };
		const got: { v: Source | null } = { v: null };
		const res = await reconcile(
			async () => {
				got.v = p.id
					? await orNull(api().source(p.id))
					: ((await api().sources({ limit: 200 })).items.find((x) => x.name === p.input.name) ?? null);
				return got.v;
			},
			(s) => compareSource(p.input, s, (x) => inputOf(draftOf(x), x.has_credentials)),
		);
		saving = false;
		if (pending !== p) return null;
		if (res.kind === 'confirmed' && got.v) {
			const saved = got.v;
			toast(reconcileText(res));
			onsaved(saved);
			// The parent hands the stored source back in, which resets the form; show the result after that.
			await tick();
			check = res;
			return saved;
		}
		check = res;
		return null;
	}
	/** Tests what the form says, saved or not; a test never saves. */
	async function test() {
		testError = '';
		blockedReasons = [];
		const p = plan;
		if (p.kind === 'blocked') {
			blockedReasons = p.reasons;
			return;
		}
		sample = null;
		testing = true;
		try {
			sample =
				p.kind === 'unsaved'
					? await api().testUnsavedSource(p.body)
					: await api().testSource(p.id, p.body ?? undefined);
		} catch (e) {
			testError = testErrorText(e);
		} finally {
			testing = false;
		}
	}
	async function readColumns(e: Event) {
		const f = (e.currentTarget as HTMLInputElement).files?.[0];
		if (!f) return;
		previewed = f;
		await previewAgain();
	}
	async function previewAgain() {
		const f = previewed;
		if (!f) return;
		preview = null;
		await guard(async () => {
			preview = await api().previewColumns(f, draft.sheet, draft.headerRow);
		}, 'Columns read. Nothing was stored.');
	}
	async function remove() {
		if (!source) return;
		let res: SourceDeleted | null = null;
		const ok = await guard(
			async () => {
				res = await api().deleteSource(source.id, purge);
			},
			() => (res ? deleteMessage(res) : 'Source deleted.'),
			'Not deleted',
		);
		if (ok) onsaved(null);
	}
</script>

{#if !canWrite}
	<Callout kind="info" title="Read only"
		>You can view this source. Editing needs the tenant_admin role.</Callout
	>
{/if}
{#if structured(draft.type)}
	<Callout kind="info" title="Ingest capability"
		>Connection tests and column previews do not prove ingest support or completed indexing. API and datatable
		sync require support from the deployed ingest service; unsupported kinds return a failed run. Confirm
		deployment support before setting up this source.</Callout
	>
{/if}
<fieldset class="editor-fields" disabled={!canWrite || saving}>
	<form
		class="form-grid"
		onsubmit={(e) => {
			e.preventDefault();
			save();
		}}
	>
		<label class="field"><span class="label">Name</span><input class="input" bind:value={draft.name} /></label
		>
		<fieldset class="field span" disabled={!!source}>
			<legend class="label">Type</legend>
			<div class="types">
				{#each SOURCE_TYPES as [t, label, hint]}
					<label class="type" class:on={draft.type === t}>
						<input type="radio" name="source-type" value={t} bind:group={draft.type} />
						<span>{label}</span><span class="cell-sub">{hint}</span>
					</label>
				{/each}
			</div>
			{#if source}<span class="cell-sub">The type is fixed once the source is saved.</span>{/if}
		</fieldset>
		{#if draft.type === 'website'}
			<fieldset class="field span">
				<legend class="label">Pages</legend>
				<div class="types">
					{#each WEBSITE_MODES as [m, label, hint]}
						<label class="type" class:on={draft.mode === m}>
							<input type="radio" name="website-mode" value={m} bind:group={draft.mode} />
							<span>{label}</span><span class="cell-sub">{hint}</span>
						</label>
					{/each}
				</div>
			</fieldset>
			{#if draft.mode === 'urls'}
				<label class="field span"
					><span class="label">URLs, one per line</span><textarea
						class="input mono"
						rows="4"
						bind:value={draft.urls}
					></textarea></label
				>
			{:else}
				<label class="field span"
					><span class="label">{draft.mode === 'crawl' ? 'Start page' : 'Sitemap URL'}</span><input
						class="input mono"
						bind:value={draft.url}
						placeholder={draft.mode === 'crawl'
							? 'https://help.example.nl/'
							: 'https://help.example.nl/sitemap.xml'}
					/></label
				>
			{/if}
			{#if draft.mode === 'crawl'}
				<label class="field"
					><span class="label">Link depth (1-5)</span><input
						class="input"
						type="number"
						min="1"
						max="5"
						step="1"
						bind:value={draft.maxDepth}
						placeholder="2"
					/></label
				>
				<label class="field"
					><span class="label">Max pages (1-200)</span><input
						class="input"
						type="number"
						min="1"
						max="200"
						step="1"
						bind:value={draft.maxPages}
						placeholder="50"
					/></label
				>
				<label class="field"
					><span class="label">Allowed domains, one per line</span><textarea
						class="input mono"
						rows="2"
						bind:value={draft.allowedDomains}
						placeholder="help.example.nl"
					></textarea><span class="cell-sub">Empty: strata_ingest's default.</span></label
				>
				<label class="field"
					><span class="label">Link query</span><input
						class="input"
						bind:value={draft.linkQuery}
						placeholder="returns and warranty"
					/><span class="cell-sub">Optional. strata_ingest uses it to choose which links to follow.</span
					></label
				>
			{:else if draft.mode === 'sitemap'}
				<label class="field"
					><span class="label">Max URLs (1-1000)</span><input
						class="input"
						type="number"
						min="1"
						max="1000"
						step="1"
						bind:value={draft.maxUrls}
						placeholder="200"
					/></label
				>
				<label class="field"
					><span class="label">Only URLs containing, one per line</span><textarea
						class="input mono"
						rows="2"
						bind:value={draft.include}
						placeholder="/help/"
					></textarea></label
				>
				<label class="field"
					><span class="label">Skip URLs containing, one per line</span><textarea
						class="input mono"
						rows="2"
						bind:value={draft.exclude}
						placeholder="/archive/"
					></textarea></label
				>
			{/if}
		{/if}
		{#if structured(draft.type)}
			<label class="field span"
				><span class="label">{draft.type === 'api' ? 'Endpoint URL' : 'Table URL'}</span><input
					class="input mono"
					bind:value={draft.url}
					placeholder="https://api.example.nl/v2/customers"
				/></label
			>
			<label class="field"
				><span class="label">Authentication</span>
				<select class="input" bind:value={draft.authType}>
					<option value="none">None</option>
					<option value="bearer">Bearer token</option>
					<option value="header">API key header</option>
				</select></label
			>
			{#if draft.authType === 'header'}
				<label class="field"
					><span class="label">Header name</span><input
						class="input mono"
						bind:value={draft.headerName}
						placeholder="X-Api-Key"
					/></label
				>
			{/if}
		{/if}
		{#if draft.type === 'datatable'}
			<label class="field"
				><span class="label">Format</span>
				<select class="input" bind:value={draft.format}>
					<option value="csv">CSV</option>
					<option value="xlsx">XLSX</option>
				</select></label
			>
			<label class="field"
				><span class="label">Sheet (XLSX)</span><input class="input" bind:value={draft.sheet} /></label
			>
			<label class="check"
				><input type="checkbox" bind:checked={draft.headerRow} onchange={previewAgain} /> First row contains column
				names</label
			>
		{/if}
		{#if draft.type === 'file'}
			<p class="muted span">You upload the files with each re-sync. care-bus does not keep them.</p>
		{/if}
		{#if uses}
			<label class="field span">
				<span class="label"
					>Credentials {#if source?.has_credentials && !draft.clearSecret}<span class="chip">set</span
						>{/if}</span
				>
				<input
					class="input mono"
					type="password"
					autocomplete="new-password"
					aria-label="Credentials secret"
					bind:value={draft.secret}
					placeholder={source?.has_credentials
						? 'Leave empty to keep the stored secret'
						: 'Paste the token or key'}
				/>
			</label>
			{#if source?.has_credentials}
				<label class="check"
					><input type="checkbox" bind:checked={draft.clearSecret} /> Remove the stored credentials</label
				>
			{/if}
			<p class="cell-sub span">
				Stored encrypted. It is never shown again; it is only sent with test connection and each re-sync.
			</p>
		{/if}
		<label class="field"
			><span class="label">Target store</span>
			<select class="input" bind:value={draft.targetStoreId}>
				<option value="" disabled>Pick a leaf store</option>
				{#each storeOptions as s}<option value={s.id}>{s.name}</option>{/each}
			</select>
			{#if storeOptions.length === 0}<span class="cell-sub"
					>No leaf stores yet. <a href="/admin/knowledge/stores">Create one under Stores</a> first.</span
				>{/if}</label
		>
		<label class="field"
			><span class="label">Schedule (cron)</span><input
				class="input mono"
				bind:value={draft.schedule}
				placeholder="0 3 * * *"
			/><span class="cell-sub"
				>Stored with the source, but care-bus does not run schedules yet. Use Re-sync to run it.</span
			></label
		>
		<label class="check"><input type="checkbox" bind:checked={draft.enabled} /> Enabled</label>
	</form>

	{#if structured(draft.type)}
		<h3>Field mapping</h3>
		<div class="row">
			{#if draft.type === 'api'}
				<button class="btn" type="button" disabled={!canWrite || testing || saving} title={why} onclick={test}
					>{testing ? 'Testing…' : 'Test connection'}</button
				>
				{#if canWrite && !testing && plan.kind !== 'blocked' && plan.note}
					<span class="muted">{plan.note}</span>
				{/if}
			{:else}
				<label class="btn" aria-disabled={!canWrite} title={why}
					>Preview columns (CSV or XLSX) <input
						class="visually-hidden"
						type="file"
						accept=".csv,.xlsx,text/csv,application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
						disabled={!canWrite}
						onchange={readColumns}
					/></label
				>
			{/if}
		</div>
		{#if blockedReasons.length}
			<Callout kind="gap" title="Cannot test yet"><span>{blockedReasons.join(' ')}</span></Callout>
		{/if}
		{#if testError}
			<Callout kind="err" title="Test failed"><span>{testError}</span></Callout>
		{/if}
		{#if sample}
			<div class="callout test-result" class:ok={sample.ok} class:err={!sample.ok} role="status">
				<strong>{sample.ok ? 'Connection works' : 'Connection failed'}</strong>
				<span
					>HTTP {sample.status ?? '—'} · {sample.detail}{sample.truncated
						? ' · body cut at 256 KB'
						: ''}</span
				>
				{#if sample.ok}
					<span class="muted"
						>{sample.fields.length
							? `Discovered fields: ${sample.fields.join(', ')}`
							: 'No fields discovered in the sample record.'}</span
					>
				{/if}
			</div>
			{#if sample.sample != null}<JsonView value={sample.sample} />{/if}
		{/if}
		{#if preview}
			<p class="muted">{preview.row_count} rows · columns: {preview.columns.join(', ')}</p>
		{/if}
		<MappingEditor
			bind:rows={draft.mapping}
			options={attributeOptions(entities)}
			fields={knownFields}
			disabled={!canWrite}
			{why}
		/>
	{/if}

	{#if secretInSettings}
		<Callout kind="err" title="That looks like a key"
			>Put keys and tokens in the credentials field; these settings are stored in clear.</Callout
		>
	{/if}
	{#if errors.length}
		<Callout kind="info" title="Before you save"><span>{errors.join(' ')}</span></Callout>
	{/if}
</fieldset>
<ReconcileStatus result={check} retry={verify} />
<div class="actions">
	<button
		class="btn primary"
		type="button"
		disabled={!canWrite || saving || errors.length > 0 || secretInSettings}
		title={why}
		onclick={save}>{saving ? 'Saving…' : source ? 'Save' : 'Add source'}</button
	>
	{#if source}
		<button class="btn" type="button" disabled={!canWrite} title={why} onclick={() => (confirmDelete = true)}
			>Delete</button
		>
	{/if}
</div>
{#if confirmDelete && source}
	<div class="callout err" role="alertdialog" aria-label="Delete source">
		<strong>Delete {source.name}?</strong>
		<span
			>{purge
				? 'Its documents are deleted from strata_rag too.'
				: 'Its documents stay in strata_rag. Only the settings and run history are removed.'}</span
		>
		<label class="check"><input type="checkbox" bind:checked={purge} /> Also delete its documents</label>
		<span
			><button class="btn sm" type="button" onclick={remove}>Delete source</button>
			<button class="btn sm" type="button" onclick={() => (confirmDelete = false)}>Keep it</button></span
		>
	</div>
{/if}

<style>
	.editor-fields {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
	}
	.span {
		grid-column: 1 / -1;
	}
	.types {
		display: grid;
		grid-template-columns: repeat(auto-fit, minmax(160px, 1fr));
		gap: 8px;
	}
	.type {
		display: grid;
		gap: 2px;
		padding: 8px;
		border: 1px solid var(--line);
		border-radius: 8px;
		cursor: pointer;
	}
	.type.on {
		border-color: var(--accent);
	}
	.check {
		display: flex;
		gap: 6px;
		align-items: center;
	}
	.test-result.ok {
		border-color: color-mix(in srgb, var(--ok) 45%, var(--line));
		background: color-mix(in srgb, var(--ok) 7%, var(--panel));
	}
	.test-result.ok strong {
		color: var(--ok);
	}
	.visually-hidden {
		position: absolute;
		width: 1px;
		height: 1px;
		overflow: hidden;
		clip: rect(0 0 0 0);
	}
</style>
