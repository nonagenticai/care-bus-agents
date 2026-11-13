<script lang="ts">
	import { page } from '$app/state';
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { cellText, changedEditable, isDemoTable, tableLabel } from '../../tables';
	import type { DemoRecord, DemoTablePage } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const table = $derived(page.params.table ?? '');
	const id = $derived(page.params.id ?? '');

	// The record route returns the bare row; the table's editable columns come from the list route (limit 1).
	const rec = new Resource<{ row: DemoRecord; meta: DemoTablePage }>(
		async () => {
			if (!isDemoTable(table)) throw new Error(`Unknown demo table "${table}"`);
			const [row, meta] = await Promise.all([
				api().demoRecord(table, id),
				api().demoRecords(table, { limit: 1 }),
			]);
			return { row, meta };
		},
		() => false,
	);
	$effect(() => {
		void table;
		void id;
		rec.refresh();
	});

	let draft = $state<DemoRecord | null>(null);
	// Reset the form to server values whenever a fresh record arrives (initial load or after a save).
	$effect(() => {
		if (rec.value.state === 'ok') draft = structuredClone($state.snapshot(rec.value.data.row));
	});

	const admin = $derived(session.canWriteDemo);
	const label = $derived(tableLabel(table));
	const dirty = $derived(
		rec.value.state === 'ok' && draft !== null
			? Object.keys(changedEditable(rec.value.data.meta, rec.value.data.row, draft)).length > 0
			: false,
	);
	const saveWhy = $derived(admin ? (dirty ? '' : 'No changes to save') : 'Needs the super_admin role');

	function setField(col: string, v: unknown) {
		if (draft) draft[col] = v;
	}

	function save() {
		if (!draft || rec.value.state !== 'ok' || !isDemoTable(table)) return;
		const changes = changedEditable(rec.value.data.meta, rec.value.data.row, draft);
		if (Object.keys(changes).length === 0) return;
		guard(() => api().saveDemoRecord(table, id, changes), `Saved ${id}.`).then((ok) => {
			if (!ok) return; // 403/409/422: keep the edits so the user can fix them
			rec.refresh();
		});
	}
</script>

<header class="page-head">
	<div>
		<h1 class="mono">{id}</h1>
		<p><a href="/demo">Demo data</a> / <a href="/demo/{table}">{label}</a></p>
	</div>
</header>

{#if rec.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if rec.value.state === 'unavailable'}
	<Unavailable reason={rec.value.reason} retry={() => rec.refresh()} />
{:else if rec.value.state === 'ok' && draft}
	{@const editable = rec.value.data.meta.editable}
	<div class="grid g2">
		<Card title={editable.length ? 'Edit' : 'Read-only table'}>
			{#if editable.length === 0}
				<p class="muted">demo-backends does not allow edits to {label.toLowerCase()}.</p>
			{:else}
				<form
					class="stack"
					onsubmit={(e) => {
						e.preventDefault();
						save();
					}}
				>
					{#each editable as f (f)}
						<label class="field">
							<span class="label">{f}</span>
							{#if typeof draft[f] === 'number'}
								<input
									class="input"
									type="number"
									step="any"
									value={draft[f] as number}
									oninput={(e) => {
										const n = (e.currentTarget as HTMLInputElement).valueAsNumber;
										setField(f, Number.isNaN(n) ? null : n);
									}}
									disabled={!admin}
									title={admin ? '' : 'Needs the super_admin role'}
								/>
							{:else}
								<input
									class="input"
									value={String(draft[f] ?? '')}
									oninput={(e) => {
										const v = (e.currentTarget as HTMLInputElement).value;
										setField(f, v === '' ? null : v);
									}}
									disabled={!admin}
									title={admin ? '' : 'Needs the super_admin role'}
								/>
							{/if}
						</label>
					{/each}
					<div class="actions">
						<button class="btn primary" type="submit" disabled={!admin || !dirty} title={saveWhy}>Save</button
						>
					</div>
				</form>
			{/if}
		</Card>
		<Card title="Record"
			><KeyValues
				pairs={Object.entries(rec.value.data.row).map(([k, v]) => [k, cellText(v)] as [string, string])}
			/></Card
		>
	</div>
{/if}
