<script lang="ts">
	import Freshness from '$lib/ui/Freshness.svelte';
	import { api, guard, guardOutcome } from '$lib/app.svelte';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		CONNECTOR_FAULTS,
		compareConnector,
		faultyMappingRows,
		fieldFault,
		reconcile,
		reconcileText,
		type ConnectorFaultField,
		type FieldFault,
		type Reconciliation,
	} from '$lib/write-outcome';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import { looksLikeSecret } from '$lib/api/client';
	import { toConnectorInput } from './connector-form';
	import type { Connector, ConnectorInput, Paged } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Status from '$lib/ui/Status.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import ReconcileStatus from '$lib/ui/ReconcileStatus.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import { fmtValue } from '$lib/refinements';

	const FAULT_FIELDS: ConnectorFaultField[] = [
		'base_url',
		'auth_type',
		'secret_ref',
		'timeout_ms',
		'mappings',
	];

	const conns = new Resource<Paged<Connector>>(() => api().connectors());
	conns.refresh();
	const canWrite = $derived(session.can('tenant_admin'));
	let open = $state(false);
	let draft = $state<Connector | null>(null);
	let original = $state('');
	const dirty = $derived(draft !== null && JSON.stringify(toConnectorInput(draft)) !== original);
	let testMsg = $state('');
	let saving = $state(false);
	let testing = $state(false);
	/** The last refusal; shown next to its field when its code names one, else above the actions. */
	let fault = $state<FieldFault<ConnectorFaultField> | null>(null);
	const badRows = $derived(draft ? faultyMappingRows(fault, draft.mappings) : []);
	/** The re-read after a save whose answer was lost, and what that save sent (Check again compares with it). */
	let check = $state<Reconciliation | null>(null);
	let pending: { input: ConnectorInput; name: string } | null = null;
	const faultAttrs = (f: ConnectorFaultField) =>
		fault?.field === f ? { 'aria-invalid': 'true' as const, 'aria-describedby': `be-err-${f}` } : {};
	const rowAttrs = (i: number) =>
		badRows.includes(i) ? { 'aria-invalid': 'true' as const, 'aria-describedby': 'be-err-mappings' } : {};
	const secretError = $derived(
		draft?.secret_ref && looksLikeSecret(draft.secret_ref)
			? 'This looks like a key, not a reference. Enter the secret name, for example carebus-connectors/oms-api-key.'
			: '',
	);

	/** The release's (base) version of the open connector, read when it refines or is a base slot (plan 14). */
	let baseConn = $state<Connector | null>(null);
	let reverting = $state(false);
	async function loadBase(id: string) {
		try {
			const b = (await api().connectors('base')).items.find((x) => x.id === id) ?? null;
			if (draft?.id === id) baseConn = b;
		} catch {
			// Without the base side the drawer still edits; it just cannot show the base values.
		}
	}
	/** The base value of `f` when the open connector refines it and the value differs from the form's. */
	function baseHint(f: ConnectorFaultField): string | null {
		if (!draft?.overridden || !baseConn) return null;
		const b = baseConn[f];
		return JSON.stringify(b ?? null) === JSON.stringify(draft[f] ?? null) ? null : fmtValue(b);
	}
	/** Drop this tenant's refinement: the connector is the release's again. */
	async function revertToBase() {
		if (!draft || reverting || !canWrite) return;
		reverting = true;
		const id = draft.id;
		const ok = await guard(
			() => api().refine('revert', 'connector', { id }),
			`Reverted ${draft.name} to the release's values.`,
			'Not reverted',
		);
		reverting = false;
		if (ok) {
			open = false;
			await conns.refresh();
		}
	}

	function edit(c: Connector) {
		baseConn = null;
		if (c.overridden || c.origin === 'base') void loadBase(c.id);
		draft = structuredClone($state.snapshot(c));
		original = JSON.stringify(toConnectorInput(draft));
		testMsg = '';
		fault = null;
		check = null;
		pending = null;
		open = true;
	}
	// The test runs the SAVED connector (the API takes no body), so label it that way.
	async function test() {
		if (!draft || testing || saving || !canWrite) return;
		testing = true;
		testMsg = 'Testing the saved connector…';
		try {
			const t = await api().testConnector(draft.id);
			testMsg = `${t.ok ? 'OK' : 'Failed'}${t.status != null ? ` (HTTP ${t.status})` : ''}: ${t.detail} · ${fmt.ms(t.latency_ms)}`;
			await conns.refresh();
		} catch (e) {
			testMsg = `Test failed: ${(e as Error).message}`;
		} finally {
			testing = false;
		}
	}
	async function save() {
		if (!draft || saving || !canWrite || secretError) return;
		saving = true;
		fault = null;
		check = null;
		const name = draft.name;
		const input = toConnectorInput($state.snapshot(draft) as Connector);
		const r = await guardOutcome(() => api().saveConnector(input), `Saved ${name}.`, 'Not saved', {
			reconcile: true,
		});
		if (r.kind === 'ok') {
			saving = false;
			open = false;
			await conns.refresh();
		} else if (r.kind === 'rejected') {
			saving = false;
			fault = fieldFault(r.error, CONNECTOR_FAULTS, FAULT_FIELDS);
		} else {
			pending = { input, name };
			await verify();
		}
	}
	/** After a lost answer: re-read the connector and compare it with what was sent. Never resubmits. */
	async function verify() {
		const p = pending;
		if (!p) return;
		saving = true;
		check = { kind: 'checking' };
		// The API has no single-connector read in the client; the list carries the same row.
		const res = await reconcile(
			async () => (await api().connectors()).items.find((c) => c.id === p.input.id) ?? null,
			(stored) => compareConnector(p.input, stored),
		);
		saving = false;
		if (pending !== p) return;
		check = res;
		if (res.kind === 'confirmed') {
			// The drawer closes, so the result goes to the toast.
			check = null;
			toast(reconcileText(res));
			pending = null;
			open = false;
			await conns.refresh();
		}
	}
</script>

<header class="page-head">
	<div>
		<h1>Backend systems</h1>
		<p>Connectors the MCP tools call. In the demo these point at demo-backends.</p>
	</div>
</header>
<Freshness resource={conns} />

{#if conns.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if conns.value.state === 'unavailable'}
	<Unavailable reason={conns.value.reason} retry={() => conns.refresh()} />
{:else if conns.value.state === 'empty'}
	<div class="empty">No connectors yet.</div>
{:else}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl clickable">
				<thead>
					<tr
						><th>Name</th><th>System</th><th>Base URL</th><th>Auth</th><th class="r">Timeout</th><th
							>Health</th
						></tr
					>
				</thead>
				<tbody>
					{#each conns.value.data.items as c}
						<tr
							><td
								><button type="button" class="table-action" onclick={() => edit(c)}>{c.name}</button
								>{#if c.overridden}
									<Chip
										text="Refined"
										variant="accent"
										title="Your values override the release's"
									/>{:else if c.origin === 'tenant'}
									<Chip text="Added" variant="ok" title="Not in the release" />{/if}</td
							>
							<td>{c.system}</td>
							<td class="mono">{c.base_url}</td>
							<td>{c.auth_type}</td>
							<td class="r">{fmt.ms(c.timeout_ms)}</td>
							<td
								><Status
									word={c.health}
									label={c.health === 'unknown' ? 'Not tested' : c.health === 'ok' ? 'Healthy' : 'Down'}
								/></td
							>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}

<Drawer bind:open title={draft?.name ?? ''} wide {dirty}>
	{#if draft}
		{#if !canWrite}
			<Callout kind="info" title="Read only"
				>You can view this connector. Editing needs the tenant_admin role.</Callout
			>
		{/if}
		{#if draft.overridden}
			<Callout kind="info" title="Refined"
				>Your values override the release's. Fields you changed show the release's value beside them; Revert
				to base drops your refinement.</Callout
			>
		{:else if draft.origin === 'base'}
			<Callout kind="info" title="From the release"
				>This connector comes from the job's release. Saving creates your refinement; the release's values
				stay beside it.</Callout
			>
		{/if}
		<fieldset class="be-fields" disabled={!canWrite || saving}>
			<div class="form-grid">
				<label class="field"
					><span class="label">Base URL</span><input
						class="input mono"
						bind:value={draft.base_url}
						{...faultAttrs('base_url')}
					/>{#if baseHint('base_url')}<span class="base-hint" data-testid="base-base_url"
							>Base: <span class="mono">{baseHint('base_url')}</span></span
						>{/if}{#if fault?.field === 'base_url'}<span class="field-err" id="be-err-base_url"
							>{fault.message}</span
						>{/if}</label
				>
				<label class="field"
					><span class="label">Auth type</span>
					<select class="input" bind:value={draft.auth_type} {...faultAttrs('auth_type')}>
						<option value="oauth2_client_credentials">OAuth2 client credentials</option>
						<option value="api_key">API key</option>
						<option value="mtls_api_key">mTLS + API key</option>
						<option value="none">None</option>
					</select>{#if baseHint('auth_type')}<span class="base-hint" data-testid="base-auth_type"
							>Base: <span class="mono">{baseHint('auth_type')}</span></span
						>{/if}{#if fault?.field === 'auth_type'}<span class="field-err" id="be-err-auth_type"
							>{fault.message}</span
						>{/if}
				</label>
				<label class="field"
					><span class="label">Secret reference</span><input
						class="input mono"
						bind:value={draft.secret_ref}
						placeholder="carebus-demo/backends-token"
						autocomplete="off"
						spellcheck="false"
						{...faultAttrs('secret_ref')}
					/>{#if baseHint('secret_ref')}<span class="base-hint" data-testid="base-secret_ref"
							>Base: <span class="mono">{baseHint('secret_ref')}</span></span
						>{/if}{#if fault?.field === 'secret_ref'}<span class="field-err" id="be-err-secret_ref"
							>{fault.message}</span
						>{/if}</label
				>
				<label class="field"
					><span class="label">Timeout (ms)</span><input
						class="input"
						type="number"
						min="100"
						bind:value={draft.timeout_ms}
						{...faultAttrs('timeout_ms')}
					/>{#if baseHint('timeout_ms')}<span class="base-hint" data-testid="base-timeout_ms"
							>Base: <span class="mono">{baseHint('timeout_ms')}</span></span
						>{/if}{#if fault?.field === 'timeout_ms'}<span class="field-err" id="be-err-timeout_ms"
							>{fault.message}</span
						>{/if}</label
				>
			</div>
			{#if secretError}<Callout kind="err" title="Secret reference">{secretError}</Callout>{/if}
			<h3>Field mapping</h3>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead
						><tr><th>Backend field</th><th>Ontology attribute</th><th aria-label="Remove"></th></tr></thead
					>
					<tbody>
						{#each draft.mappings as m, i}
							<tr>
								<td
									><input
										class="input mono"
										bind:value={m.backend_field}
										aria-label="Backend field"
										{...rowAttrs(i)}
									/></td
								>
								<td
									><input
										class="input mono"
										bind:value={m.attribute}
										aria-label="Ontology attribute (Entity.attribute)"
										placeholder="Order.order_id"
										{...rowAttrs(i)}
									/></td
								>
								<td
									><button
										class="btn sm"
										type="button"
										aria-label={`Remove mapping ${m.backend_field || i + 1}`}
										onclick={() => draft && (draft.mappings = draft.mappings.filter((_, j) => j !== i))}
										>Remove</button
									></td
								>
							</tr>
						{:else}
							<tr><td colspan="3" class="muted">No field mappings.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
			{#if fault?.field === 'mappings'}<span class="field-err" id="be-err-mappings">{fault.message}</span
				>{/if}
		</fieldset>
		{#if fault && !fault.field}
			<Callout kind="err" title="Not saved">{fault.message} ({fault.code})</Callout>
		{/if}
		<ReconcileStatus result={check} retry={verify} />
		{#if testMsg}<p class="mono">{testMsg}</p>{/if}
		<div class="actions">
			<button
				class="btn"
				type="button"
				disabled={!canWrite || saving || testing}
				title={canWrite ? '' : 'Needs the tenant_admin role'}
				onclick={() => draft && (draft.mappings = [...draft.mappings, { backend_field: '', attribute: '' }])}
				>Add mapping</button
			>
			<button
				class="btn"
				type="button"
				disabled={!canWrite || saving || testing}
				title={canWrite ? '' : 'Needs the tenant_admin role'}
				onclick={test}>Test saved connector</button
			>
			{#if draft.overridden}
				<button
					class="btn"
					type="button"
					disabled={!canWrite || saving || reverting}
					title={canWrite
						? "Drop your refinement; the release's values apply"
						: 'Needs the tenant_admin role'}
					onclick={revertToBase}>{reverting ? 'Reverting…' : 'Revert to base'}</button
				>
			{/if}
			<button
				class="btn primary"
				type="button"
				disabled={!canWrite || saving || secretError !== ''}
				title={!canWrite ? 'Needs the tenant_admin role' : secretError}
				onclick={save}>{saving ? 'Saving…' : 'Save'}</button
			>
		</div>
	{/if}
</Drawer>

<style>
	.base-hint {
		color: var(--muted);
		font-size: 12px;
	}
	.field-err {
		color: var(--err);
		font-size: 12px;
	}
	.be-fields {
		border: 0;
		padding: 0;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 12px;
	}
</style>
