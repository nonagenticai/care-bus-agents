<script lang="ts">
	import { onMount } from 'svelte';
	import { app, loadTenants, switchTenant } from '$lib/app.svelte';
	import { filterTenants } from '$lib/tenants';
	import Chip from './Chip.svelte';

	/** A super_admin chooses the tenant every page acts in (X-Active-Tenant-Id). Plan 14: replaces the uuid input. */
	let { onpicked }: { onpicked?: () => void } = $props();
	let q = $state('');
	let listError = $state<string | null>(null);
	let error = $state('');
	let switching = $state(false);
	let typed = $state('');
	const shown = $derived(filterTenants(app.tenants, q));
	let loaded = $state(false);
	onMount(() => {
		void loadTenants().then((e) => {
			listError = e;
			loaded = true;
		});
	});

	async function pick(id: string) {
		if (switching) return;
		switching = true;
		error = (await switchTenant(id)) ?? '';
		switching = false;
		if (!error) {
			typed = '';
			onpicked?.();
		}
	}
</script>

<div class="picker" data-testid="tenant-picker">
	<label class="field"
		><span class="label">Find a tenant</span><input
			class="input"
			type="search"
			placeholder="Name or id"
			autocomplete="off"
			bind:value={q}
		/></label
	>
	{#if listError}
		<p class="err" role="alert">The tenant list is unavailable ({listError}). Enter the tenant id below.</p>
	{:else if app.tenants.length === 0}
		<p class="muted">{loaded ? 'No tenants yet.' : 'Loading tenants…'}</p>
	{:else}
		<ul class="list-plain tenants" aria-label="Tenants">
			{#each shown as t (t.id)}
				<li>
					<button
						class="tenant"
						type="button"
						aria-current={t.id === app.activeTenant ? 'true' : undefined}
						disabled={switching}
						onclick={() => pick(t.id)}
						><b>{t.name}</b>
						{#if t.status === 'suspended'}<Chip text="Suspended" variant="warn" />{/if}
						{#if t.id === app.activeTenant}<Chip text="Current" variant="accent" />{/if}
						<span class="mono muted">{t.id}</span></button
					>
				</li>
			{:else}
				<li class="muted">No tenant matches “{q}”.</li>
			{/each}
		</ul>
	{/if}
	<form
		class="typed"
		onsubmit={(e) => {
			e.preventDefault();
			void pick(typed);
		}}
	>
		<label class="field"
			><span class="label">Or enter a tenant id</span><input
				class="input mono"
				placeholder="tenant uuid"
				autocomplete="off"
				spellcheck="false"
				bind:value={typed}
			/></label
		>
		<button class="btn sm" type="submit" disabled={switching || !typed.trim()}>Act in tenant</button>
	</form>
	{#if error}<p class="err" role="alert">{error}</p>{/if}
</div>

<style>
	.picker {
		display: grid;
		gap: 12px;
		max-width: 60ch;
	}
	.tenants {
		display: grid;
		gap: 4px;
		max-height: 50vh;
		overflow: auto;
	}
	.tenant {
		display: flex;
		flex-wrap: wrap;
		align-items: center;
		gap: 8px;
		width: 100%;
		text-align: left;
		padding: 8px 10px;
		border: 1px solid var(--line);
		border-radius: 6px;
		background: var(--panel);
		color: inherit;
		cursor: pointer;
	}
	.tenant[aria-current='true'] {
		border-color: var(--accent);
	}
	.typed {
		display: flex;
		align-items: flex-end;
		gap: 8px;
		flex-wrap: wrap;
	}
	.err {
		color: var(--err);
	}
</style>
