<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import {
		ACTION_LABEL,
		STATE_LABEL,
		actionsFor,
		fmtValue,
		sideBySide,
		type SideObject,
		type SideRow,
	} from '$lib/refinements';
	import type { Paged, RefinementOp, ReleaseConflict } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	type Item = Record<string, unknown>;
	let {
		object,
		load,
		summary,
		conflicts,
		onchange,
		edit,
	}: {
		object: SideObject;
		/** One layer of the object's list route. */
		load: (layer: 'base' | 'tenant') => Promise<Paged<Item>>;
		/** The one-line value shown for a row that exists on one side only. */
		summary: (item: Item) => unknown;
		conflicts: ReleaseConflict[];
		/** After a refinement action: the page re-reads the conflicts. */
		onchange: () => void;
		/** Opens the page's editor for a visible row (objects with an override route, e.g. context spaces). */
		edit?: (row: SideRow) => void;
	} = $props();

	const layers = new Resource<{ base: Item[]; tenant: Item[] }>(
		async () => {
			const [b, t] = await Promise.all([load('base'), load('tenant')]);
			return { base: b.items, tenant: t.items };
		},
		(d) => d.base.length === 0 && d.tenant.length === 0,
	);
	layers.refresh();
	const canWrite = $derived(session.can('tenant_admin'));
	const rows = $derived(
		layers.value.state === 'ok'
			? sideBySide(object, layers.value.data.base, layers.value.data.tenant, conflicts)
			: [],
	);
	let busy = $state('');
	const VARIANT = { base: 'muted', override: 'accent', extension: 'ok', tombstone: 'warn' } as const;

	async function act(row: SideRow, op: RefinementOp) {
		if (busy) return;
		busy = `${row.keyText}:${op}`;
		const ok = await guard(
			() => api().refine(op, row.object, row.key),
			`${ACTION_LABEL[op]}: ${row.keyText}.`,
			'Not changed',
		);
		busy = '';
		if (ok) {
			await layers.refresh();
			onchange();
		}
	}
</script>

<Freshness resource={layers} />
{#if layers.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if layers.value.state === 'unavailable'}
	<Unavailable reason={layers.value.reason} retry={layers.retry} />
{:else if layers.value.state === 'empty'}
	<div class="empty">Neither the release nor this tenant defines any.</div>
{:else}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl" data-testid="side-by-side-{object}">
				<thead>
					<tr
						><th>Key</th><th>State</th><th>Base (release)</th><th>Yours</th><th aria-label="Actions"></th></tr
					>
				</thead>
				<tbody>
					{#each rows as row (row.keyText)}
						<tr data-key={row.keyText}>
							<td class="mono">{row.keyText}</td>
							<td>
								<Chip text={STATE_LABEL[row.state]} variant={VARIANT[row.state]} />
								{#if row.conflict}<Chip
										text="Release conflict"
										variant="err"
										title={row.conflict.kind}
									/>{/if}
							</td>
							{#if row.state === 'override'}
								<td>
									{#each row.diff as d}<div>
											<span class="muted">{d.field}:</span> <span class="mono">{d.base}</span>
										</div>
									{:else}<span class="muted">Same as yours</span>{/each}
								</td>
								<td>
									{#each row.diff as d}<div>
											<span class="muted">{d.field}:</span> <span class="mono">{d.tenant}</span>
										</div>
									{:else}<span class="muted">No changes</span>{/each}
								</td>
							{:else if row.state === 'base'}
								<td class="mono">{fmtValue(summary(row.base!))}</td>
								<td class="muted">Uses the base</td>
							{:else if row.state === 'extension'}
								<td class="muted">Not in the release</td>
								<td class="mono">{fmtValue(summary(row.tenant!))}</td>
							{:else}
								<td class="mono">{row.base ? fmtValue(summary(row.base)) : 'Gone from the release'}</td>
								<td class="muted">Hidden by you</td>
							{/if}
							<td class="r">
								{#if edit && row.state !== 'tombstone'}
									<button
										class="btn sm"
										type="button"
										disabled={!canWrite || busy !== ''}
										title={canWrite ? '' : 'Needs the tenant_admin role'}
										onclick={() => edit(row)}>Edit</button
									>
								{/if}
								{#each actionsFor(row, true) as op}
									<button
										class="btn sm"
										type="button"
										disabled={!canWrite || busy !== ''}
										title={canWrite ? '' : 'Needs the tenant_admin role'}
										onclick={() => act(row, op)}>{ACTION_LABEL[op]}</button
									>
								{/each}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}
