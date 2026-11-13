<script lang="ts">
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import type { ContextSpace, ReleaseConflict } from '$lib/api/types';
	import type { SideRow } from '$lib/refinements';
	import Callout from '$lib/ui/Callout.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Tabs from '$lib/ui/Tabs.svelte';
	import ContextSpaceEditor from './ContextSpaceEditor.svelte';
	import OverlayTable from './OverlayTable.svelte';
	import SettingsTable from './SettingsTable.svelte';

	let tab = $state('connectors');
	let conflicts = $state<ReleaseConflict[]>([]);
	async function loadConflicts() {
		try {
			conflicts = (await api().releaseConflicts('open')).items;
		} catch {
			// The tables still render; conflicts show on the release page with its own error.
			conflicts = [];
		}
	}
	void loadConflicts();
	const canWrite = $derived(session.can('tenant_admin'));
	type Rows = { items: Record<string, unknown>[]; next: string | null };
	/** The list routes are typed per object; the side-by-side table reads them as plain records. */
	const page = (f: (l: 'base' | 'tenant') => Promise<unknown>) => (l: 'base' | 'tenant') =>
		f(l) as Promise<Rows>;

	// Context-space override: the row being edited, and a counter that remounts the table after a save.
	let editing = $state<SideRow | null>(null);
	let editorOpen = $state(false);
	let editorDirty = $state(false);
	let spacesVersion = $state(0);
	function editSpace(row: SideRow) {
		editing = row;
		editorDirty = false;
		editorOpen = true;
	}
	function spaceSaved() {
		editorOpen = false;
		spacesVersion++;
		void loadConflicts();
	}
</script>

<header class="page-head">
	<div>
		<h1>Base & refinements</h1>
		<p>
			The job's configuration from its current release beside this tenant's refinements. Revert drops a
			refinement, Hide removes a release object for this tenant only, Keep mine accepts a changed release
			under your refinement.
		</p>
	</div>
</header>
{#if !canWrite}
	<Callout kind="info" title="Read only"
		>You can view the refinements. Changing them needs the tenant_admin role.</Callout
	>
{/if}
<Tabs
	items={[
		['connectors', 'Connectors'],
		['spaces', 'Context spaces'],
		['entities', 'Entities'],
		['relations', 'Relations'],
		['stores', 'Knowledge stores'],
		['settings', 'Job settings'],
	]}
	bind:active={tab}
>
	{#if tab === 'connectors'}
		<OverlayTable
			object="connector"
			load={page((l) => api().connectors(l))}
			summary={(i) => i.base_url}
			{conflicts}
			onchange={loadConflicts}
		/>
	{:else if tab === 'spaces'}
		<OverlayTable
			object="context_space"
			load={page((l) => api().contextSpaces(l))}
			summary={(i) => i.label}
			{conflicts}
			onchange={loadConflicts}
		/>
	{:else if tab === 'entities'}
		<OverlayTable
			object="ontology_entity"
			load={page((l) => api().entities({ layer: l }))}
			summary={(i) => i.label}
			{conflicts}
			onchange={loadConflicts}
		/>
	{:else if tab === 'relations'}
		<OverlayTable
			object="ontology_relation"
			load={page((l) => api().relations(l))}
			summary={(i) => i.cardinality}
			{conflicts}
			onchange={loadConflicts}
		/>
	{:else if tab === 'stores'}
		<OverlayTable
			object="knowledge_store"
			load={page((l) => api().knowledgeStores(l))}
			summary={(i) => i.label}
			{conflicts}
			onchange={loadConflicts}
		/>
	{:else}
		<SettingsTable />
	{/if}
</Tabs>

<Drawer
	bind:open={editorOpen}
	title={editing ? `Context space ${editing.keyText}` : ''}
	wide
	dirty={editorDirty}
>
	{#if editing && editorOpen}
		{#key editing.keyText}
			<ContextSpaceEditor
				base={editing.base as ContextSpace | null}
				tenant={editing.tenant as ContextSpace | null}
				bind:dirty={editorDirty}
				onsaved={spaceSaved}
				oncancel={() => (editorOpen = false)}
			/>
		{/key}
	{/if}
</Drawer>
