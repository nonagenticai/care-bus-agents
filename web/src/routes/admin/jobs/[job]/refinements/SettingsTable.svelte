<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { fmtValue } from '$lib/refinements';
	import type { JobSettingsView } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	/** routes::settings::setting_key: the job settings a release and a tenant may set. */
	const KEYS = ['preamble', 'actor', 'stance'] as const;
	const settings = new Resource<JobSettingsView>(
		() => api().jobSettings(),
		() => false,
	);
	settings.refresh();
	const canWrite = $derived(session.can('tenant_admin'));
	let drafts = $state<Record<string, string>>({});
	let busy = $state(false);

	async function run(action: () => Promise<unknown>, msg: string) {
		if (busy) return;
		busy = true;
		const ok = await guard(action, msg, 'Not changed');
		busy = false;
		if (ok) {
			drafts = {};
			await settings.refresh();
		}
	}
	const has = (o: Record<string, unknown>, k: string) => Object.prototype.hasOwnProperty.call(o, k);
</script>

<Freshness resource={settings} />
{#if settings.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if settings.value.state === 'unavailable'}
	<Unavailable reason={settings.value.reason} retry={settings.retry} />
{:else if settings.value.state === 'ok'}
	{@const v = settings.value.data}
	<Card cls="flush">
		<div class="tbl-wrap">
			<table class="tbl" data-testid="job-settings">
				<thead
					><tr
						><th>Setting</th><th>Base (release)</th><th>Yours</th><th>Effective</th><th aria-label="Actions"
						></th></tr
					></thead
				>
				<tbody>
					{#each KEYS as k}
						<tr data-key={k}>
							<td class="mono">{k}</td>
							<td>{fmtValue(v.base[k])}</td>
							<td>
								<input
									class="input"
									aria-label={`Your ${k}`}
									disabled={!canWrite || busy}
									placeholder={has(v.override, k)
										? v.override[k] === null
											? 'Hidden'
											: ''
										: 'Uses the base'}
									value={drafts[k] ?? (has(v.override, k) ? (v.override[k] ?? '') : '')}
									oninput={(e) => (drafts[k] = e.currentTarget.value)}
								/>
							</td>
							<td>{fmtValue(v.effective[k])}</td>
							<td class="r">
								<button
									class="btn sm primary"
									type="button"
									disabled={!canWrite || busy || !(drafts[k] ?? '').trim()}
									title={canWrite ? '' : 'Needs the tenant_admin role'}
									onclick={() => run(() => api().saveJobSettings({ [k]: drafts[k]!.trim() }), `Saved ${k}.`)}
									>Save</button
								>
								<button
									class="btn sm"
									type="button"
									disabled={!canWrite ||
										busy ||
										v.base[k] == null ||
										(has(v.override, k) && v.override[k] === null)}
									title={canWrite ? 'Hide the base value' : 'Needs the tenant_admin role'}
									onclick={() => run(() => api().saveJobSettings({ [k]: null }), `Hid ${k}.`)}>Hide</button
								>
								<button
									class="btn sm"
									type="button"
									disabled={!canWrite || busy || !has(v.override, k)}
									title={canWrite ? '' : 'Needs the tenant_admin role'}
									onclick={() => run(() => api().revertJobSetting(k), `Reverted ${k} to the base.`)}
									>Revert to base</button
								>
							</td>
						</tr>
					{/each}
				</tbody>
			</table>
		</div>
	</Card>
{/if}
