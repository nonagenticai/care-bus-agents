<script lang="ts">
	import { jobPath } from '$lib/scope';
	import { app } from '$lib/app.svelte';
	import type { SourceMapping } from '$lib/api/types';
	import { mapAllFields, unmappedCount } from '$lib/sources';
	let {
		rows = $bindable(),
		options,
		fields,
		disabled,
		why = '',
	}: {
		rows: SourceMapping[];
		options: { entity: string; attribute: string; label: string }[];
		fields: string[];
		disabled: boolean;
		why?: string;
	} = $props();
	const listId = `source-fields-${Math.random().toString(36).slice(2)}`;
	const target = (m: SourceMapping) => (m.entity ? `${m.entity}.${m.attribute}` : '');
	const unmapped = $derived(unmappedCount(fields, rows));
	let note = $state('');
	function setTarget(i: number, v: string) {
		const [entity = '', attribute = ''] = v.split('.');
		rows[i] = { ...rows[i]!, entity, attribute };
	}
	function mapAll() {
		const r = mapAllFields(fields, rows, options);
		rows = r.rows;
		const rest = r.added - r.matched;
		note =
			`Added ${r.added} row${r.added === 1 ? '' : 's'}; ${r.matched} matched an ontology attribute by name.` +
			(rest ? ` Pick an attribute for the other ${rest}, or remove them.` : '');
	}
	function add() {
		rows = [...rows, { source_field: '', entity: '', attribute: '' }];
		note = options.length ? '' : 'Row added, but there are no ontology attributes to map it to yet.';
	}
</script>

{#if options.length === 0}
	<div class="callout gap" role="note">
		<strong>No ontology attributes yet</strong>
		<span
			>Mapped fields need an Entity.attribute target. <a href={jobPath(app.job, '/ontology')}
				>Define entities in the ontology</a
			>
			first; until then the source can be saved without a mapping.</span
		>
	</div>
{/if}
<div class="tbl-wrap">
	<table class="tbl dense">
		<thead><tr><th>Source field</th><th>Ontology attribute</th><th></th></tr></thead>
		<tbody>
			{#each rows as m, i}
				<tr>
					<td
						><input
							class="input mono"
							list={listId}
							bind:value={m.source_field}
							aria-label="Source field"
							{disabled}
						/></td
					>
					<td>
						<select
							class="input"
							value={target(m)}
							onchange={(e) => setTarget(i, e.currentTarget.value)}
							aria-label="Ontology attribute"
							{disabled}
						>
							<option value="">Pick an attribute</option>
							{#each options as o}<option value={`${o.entity}.${o.attribute}`}>{o.label}</option>{/each}
						</select>
					</td>
					<td
						><button
							type="button"
							class="btn sm"
							{disabled}
							title={why}
							onclick={() => (rows = rows.filter((_, j) => j !== i))}>Remove</button
						></td
					>
				</tr>
			{:else}
				<tr
					><td colspan="3" class="muted"
						>No fields mapped yet.{fields.length
							? ''
							: ' Test the connection (or preview columns) to discover the source fields.'}</td
					></tr
				>
			{/each}
		</tbody>
	</table>
</div>
<datalist id={listId}
	>{#each fields as f}<option value={f}></option>{/each}</datalist
>
<div class="actions">
	<button type="button" class="btn sm" {disabled} title={why} onclick={add}>Add mapping</button>
	{#if unmapped > 0}
		<button type="button" class="btn sm" {disabled} title={why} onclick={mapAll}
			>Map all {unmapped} field{unmapped === 1 ? '' : 's'}</button
		>
	{/if}
	{#if note}<span class="muted" role="status">{note}</span>{/if}
</div>
