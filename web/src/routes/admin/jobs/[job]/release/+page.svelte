<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { ApiError } from '$lib/api/client';
	import { Resource } from '$lib/load.svelte';
	import { fmt } from '$lib/format';
	import {
		CONFLICT_ACTION_LABEL,
		OBJECT_LABEL,
		conflictActions,
		conflictKeyText,
		conflictText,
	} from '$lib/conflicts';
	import type { Paged, RefinementOp, ReleaseConflict, ReleaseReport } from '$lib/api/types';
	import Callout from '$lib/ui/Callout.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import KeyValues from '$lib/ui/KeyValues.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	/** null = the job never had a release (404 `release`). */
	const release = new Resource<ReleaseReport | null>(
		async () => {
			try {
				return await api().currentRelease();
			} catch (e) {
				if (e instanceof ApiError && e.status === 404) return null;
				throw e;
			}
		},
		(r) => r === null,
	);
	release.refresh();
	let status = $state<'open' | 'all'>('open');
	const conflicts = new Resource<Paged<ReleaseConflict>>(() => api().releaseConflicts(status));
	$effect(() => {
		void status;
		conflicts.refresh();
	});
	const canWrite = $derived(session.can('tenant_admin'));
	let busy = $state(false);
	const when = (iso: string | null) => (iso ? new Date(iso).toLocaleString('en-GB') : '—');

	async function resolve(c: ReleaseConflict, op: RefinementOp) {
		if (busy) return;
		busy = true;
		const ok = await guard(
			() => api().refine(op, c.object, c.key),
			`${CONFLICT_ACTION_LABEL[op]}: ${conflictKeyText(c)}.`,
			'Not resolved',
		);
		busy = false;
		if (ok) await Promise.all([conflicts.refresh(), release.refresh()]);
	}
</script>

<header class="page-head">
	<div>
		<h1>Release & conflicts</h1>
		<p>
			The release the configurator pushed for this job, and where it collides with this tenant's refinements.
			Accept base drops your refinement; Keep mine keeps it on top of the new release.
		</p>
	</div>
</header>
{#if !canWrite}
	<Callout kind="info" title="Read only"
		>You can view the conflicts. Resolving them needs the tenant_admin role.</Callout
	>
{/if}

<Freshness resource={release} />
{#if release.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if release.value.state === 'unavailable'}
	<Unavailable reason={release.value.reason} retry={release.retry} />
{:else if release.value.state === 'empty'}
	<div class="empty">No release yet. The configurator has not pushed one for this job.</div>
{:else}
	{@const r = release.value.data!}
	<Card title="Current release" sub={r.release_id}>
		<KeyValues
			pairs={[
				['Release', r.release_id],
				['Applied', `${when(r.applied_at)} by ${r.applied_by}`],
				['Previous release', r.previous_release_id ?? '—'],
				['Open conflicts', fmt.n(r.conflicts.length)],
				[
					'Objects',
					Object.entries(r.counts)
						.filter(([, n]) => n > 0)
						.map(([k, n]) => `${k.replaceAll('_', ' ')} ${n}`)
						.join(' · ') || '—',
				],
			]}
		/>
	</Card>
{/if}

<Card title="Conflicts" sub="Release objects that collide with your refinements">
	{#snippet actions()}
		<Seg
			items={[
				['open', 'Open'],
				['all', 'All'],
			]}
			bind:active={status}
			label="Conflicts to show"
		/>
	{/snippet}
	{#if conflicts.value.state === 'loading'}
		<p class="muted">Loading…</p>
	{:else if conflicts.value.state === 'unavailable'}
		<Unavailable reason={conflicts.value.reason} retry={conflicts.retry} />
	{:else if conflicts.value.state === 'empty'}
		<div class="empty">{status === 'open' ? 'No open conflicts.' : 'No conflicts yet.'}</div>
	{:else}
		<div class="tbl-wrap">
			<table class="tbl" data-testid="release-conflicts">
				<thead>
					<tr
						><th>Object</th><th>Key</th><th>Conflict</th><th>Release</th><th>Detected</th><th>Resolution</th
						><th aria-label="Actions"></th></tr
					>
				</thead>
				<tbody>
					{#each conflicts.value.data.items as c (c.id)}
						<tr data-conflict={c.id}>
							<td>{OBJECT_LABEL[c.object] ?? c.object}</td>
							<td class="mono">{conflictKeyText(c)}</td>
							<td
								><Chip text={c.kind.replaceAll('_', ' ')} variant={c.resolved_at ? 'muted' : 'warn'} />
								{conflictText(c)}</td
							>
							<td class="mono">{c.release_id}</td>
							<td>{when(c.detected_at)}</td>
							<td>{c.resolution ? `${c.resolution} by ${c.resolved_by ?? '—'}` : 'Open'}</td>
							<td class="r">
								{#each conflictActions(c, true) as op}
									<button
										class="btn sm"
										type="button"
										disabled={!canWrite || busy}
										title={canWrite ? '' : 'Needs the tenant_admin role'}
										onclick={() => resolve(c, op)}>{CONFLICT_ACTION_LABEL[op]}</button
									>
								{/each}
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	{/if}
</Card>
