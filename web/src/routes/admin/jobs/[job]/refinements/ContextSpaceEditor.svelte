<script lang="ts">
	import { untrack } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import type { ContextSpace } from '$lib/api/types';
	import { toast } from '$lib/ui/toaster.svelte';
	import {
		SPACE_FIELD_LABEL,
		SPACE_ROUTING,
		fmtValue,
		spaceDraft,
		spaceFormErrors,
		spacePatch,
		type SpaceDraft,
	} from '$lib/refinements';

	let {
		base,
		tenant,
		dirty = $bindable(false),
		onsaved,
		oncancel,
	}: {
		/** The release's row (null for a space of the tenant's own). */
		base: ContextSpace | null;
		/** This tenant's row (null when the space uses the base). */
		tenant: ContextSpace | null;
		dirty?: boolean;
		onsaved: () => void;
		oncancel: () => void;
	} = $props();

	// The values in force now: the tenant row when there is one, else the base row.
	const current = $derived((tenant ?? base)!);
	// The form starts from the current values; the page remounts the editor per row.
	let draft = $state<SpaceDraft>(untrack(() => spaceDraft((tenant ?? base)!)));
	const patch = $derived(spacePatch(current, draft));
	const errors = $derived(spaceFormErrors(draft));
	$effect(() => {
		dirty = Object.keys(patch).length > 0;
	});

	let entityNames = $state<string[]>([]);
	void api()
		.entities()
		.then((p) => (entityNames = p.items.map((e) => e.name)))
		.catch(() => (entityNames = []));
	// Entities the space names stay listed even if the ontology no longer has them (the server rejects those).
	const choices = $derived([...new Set([...entityNames, ...draft.entities])].sort());

	let saving = $state(false);
	let apiError = $state('');
	async function save() {
		if (saving || errors.length || !dirty) return;
		saving = true;
		apiError = '';
		try {
			await api().saveContextSpace(current.key, patch);
			toast(`Saved your version of ${current.key}.`);
			dirty = false;
			onsaved();
		} catch (e) {
			apiError = e instanceof ApiError ? e.message : 'The change could not be saved.';
		} finally {
			saving = false;
		}
	}
	function toggle(name: string, on: boolean) {
		draft.entities = on ? [...draft.entities, name] : draft.entities.filter((x) => x !== name);
	}
</script>

<form
	class="stack"
	novalidate
	data-testid="context-space-editor"
	onsubmit={(e) => {
		e.preventDefault();
		save();
	}}
>
	<p class="muted">
		Your version replaces the release's for this tenant only. Revert on the table brings the release's back; a
		new release that changes this space is flagged as a conflict.
	</p>
	<div class="tbl-wrap">
		<table class="tbl dense">
			<thead><tr><th>Field</th><th>Base (release)</th><th>Yours</th></tr></thead>
			<tbody>
				{#each SPACE_ROUTING as f}
					<tr>
						<td>{SPACE_FIELD_LABEL[f]}</td>
						<td class="mono">{fmtValue(base?.[f])}</td>
						<td class="muted" title="Routes the space; changes only with a release"
							><span class="mono">{fmtValue(current[f])}</span> · from the release</td
						>
					</tr>
				{/each}
				<tr>
					<td><label for="cs-label">{SPACE_FIELD_LABEL.label}</label></td>
					<td>{fmtValue(base?.label)}</td>
					<td><input id="cs-label" class="input" bind:value={draft.label} autocomplete="off" /></td>
				</tr>
				<tr>
					<td><label for="cs-required">{SPACE_FIELD_LABEL.required}</label></td>
					<td class="mono">{fmtValue(base?.required)}</td>
					<td>
						<input
							id="cs-required"
							class="input mono"
							bind:value={draft.required}
							placeholder="order_id, reason"
							autocomplete="off"
						/>
						<span class="muted small">Comma separated; what the agent asks before it acts.</span>
					</td>
				</tr>
				<tr>
					<td>{SPACE_FIELD_LABEL.entities}</td>
					<td class="mono">{fmtValue(base?.entities)}</td>
					<td>
						<fieldset class="choices" aria-label={SPACE_FIELD_LABEL.entities}>
							{#each choices as name (name)}
								<label class="choice"
									><input
										type="checkbox"
										checked={draft.entities.includes(name)}
										onchange={(e) => toggle(name, e.currentTarget.checked)}
									/>
									<span class="mono">{name}</span></label
								>
							{:else}<span class="muted">No entities</span>{/each}
						</fieldset>
					</td>
				</tr>
			</tbody>
		</table>
	</div>
	{#if errors.length}
		<div class="callout err" role="alert">
			<strong>Fix these first</strong>
			<span>{errors.join(' ')}</span>
		</div>
	{/if}
	{#if apiError}
		<div class="callout err" role="alert"><strong>Not saved</strong><span>{apiError}</span></div>
	{/if}
	<div class="row">
		<button type="submit" class="btn primary" disabled={saving || !dirty || errors.length > 0}
			>{saving ? 'Saving…' : 'Save my version'}</button
		>
		<button type="button" class="btn" onclick={oncancel}>Cancel</button>
	</div>
</form>

<style>
	.choices {
		border: 0;
		margin: 0;
		padding: 0;
		display: flex;
		flex-wrap: wrap;
		gap: 4px 12px;
	}
	.choice {
		display: inline-flex;
		align-items: center;
		gap: 4px;
	}
	.small {
		display: block;
		font-size: 12px;
	}
</style>
