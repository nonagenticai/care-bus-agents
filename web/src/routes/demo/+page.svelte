<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import { armedFaults, FAULT_TOOLS, faultToolLabel, TABLES } from './tables';
	import type { FaultInput, FaultState, ScenarioList } from '$lib/api/types';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';

	const scenarios = new Resource<ScenarioList>(
		() => api().demoScenarios(),
		(l) => l.scenarios.length === 0,
	);
	const faults = new Resource<FaultState>(
		() => api().faults(),
		() => false,
	);
	scenarios.refresh();
	faults.refresh();

	const DEFAULT_SEED = 42; // P00 DEMO_SEED (F21)
	let seed = $state(DEFAULT_SEED);
	let confirmReset = $state(false);
	let confirmScenario = $state<string | null>(null);
	const admin = $derived(session.canWriteDemo);
	const denied = 'Needs the super_admin role';

	let faultTool = $state('delivery_track_shipment');
	let faultMode = $state<FaultInput['mode']>('timeout');
	let faultCount = $state(1);
</script>

<header class="page-head">
	<div>
		<h1>Demo data</h1>
		<p>Fake CRM, order, carrier and payment data served by demo-backends. Change it here to stage a demo.</p>
	</div>
</header>
<Callout kind="info" title="This is demo data">
	Every record on these pages is generated. Resetting reseeds the same data from the seed, including customer
	C-10482 used by the console script.
</Callout>

<div class="grid g4">
	{#each TABLES as t (t.id)}
		<a class="card dm-tile" href="/demo/{t.id}"><b>{t.label}</b><span class="muted mono">{t.id}</span></a>
	{/each}
</div>

<div class="grid g2">
	<Card title="Scenarios" sub="Each scenario changes records so the next conversation shows a specific path">
		{#if scenarios.value.state === 'unavailable'}
			<Unavailable reason={scenarios.value.reason} retry={() => scenarios.refresh()} />
		{:else if scenarios.value.state === 'ok'}
			<ul class="list-plain">
				{#each scenarios.value.data.scenarios as s (s.name)}
					<li>
						<span>
							<span class="cell-title mono">{s.name}</span>
							<span class="cell-sub">{s.description}</span>
						</span>
						{#if confirmScenario !== s.name}
							<button
								class="btn sm"
								type="button"
								disabled={!admin}
								title={admin ? '' : denied}
								onclick={() => (confirmScenario = s.name)}>Run…</button
							>
						{:else}
							<span class="actions" role="group" aria-label="Confirm {s.name}">
								<span>Changes demo records. Run it?</span>
								<button
									class="btn sm"
									type="button"
									onclick={() =>
										guard(() => api().runScenario(s.name), `Scenario ${s.name} is set up.`).then((ok) => {
											if (ok) confirmScenario = null;
										})}>Yes, run</button
								>
								<button class="btn sm" type="button" onclick={() => (confirmScenario = null)}>Cancel</button>
							</span>
						{/if}
					</li>
				{/each}
			</ul>
		{:else if scenarios.value.state === 'empty'}
			<div class="empty">No scenarios defined.</div>
		{:else}
			<p class="muted">Loading…</p>
		{/if}
	</Card>

	<Card title="Fault injection" sub="Make the next tool calls fail so retries and dead letters can be shown">
		<div class="form-grid">
			<label class="field">
				<span class="label">Tool</span>
				<select class="input" bind:value={faultTool} disabled={!admin} title={admin ? '' : denied}>
					{#each FAULT_TOOLS as t (t)}<option value={t}>{faultToolLabel(t)}</option>{/each}
				</select>
			</label>
			<label class="field">
				<span class="label">Mode</span>
				<select class="input" bind:value={faultMode} disabled={!admin} title={admin ? '' : denied}>
					<option value="none">none (clear)</option>
					<option value="timeout">timeout</option>
					<option value="error">error</option>
				</select>
			</label>
			<label class="field">
				<span class="label">Count</span>
				<input
					class="input"
					type="number"
					min="0"
					max="20"
					bind:value={faultCount}
					disabled={!admin}
					title={admin ? '' : denied}
				/>
			</label>
		</div>
		<div class="actions">
			<button
				class="btn"
				type="button"
				disabled={!admin}
				title={admin ? '' : denied}
				onclick={() =>
					guard(
						() => api().setFault({ tool: faultTool, mode: faultMode, count: faultCount }),
						faultMode === 'none'
							? `Cleared faults for ${faultToolLabel(faultTool)}.`
							: `Next ${faultCount} ${faultToolLabel(faultTool)} call(s): ${faultMode}.`,
					).then((ok) => {
						if (!ok) return;
						faults.refresh();
					})}>Apply</button
			>
		</div>
		{#if faults.value.state === 'ok'}
			<ul class="list-plain">
				{#each armedFaults(faults.value.data) as f (f.tool)}
					<li><span>{faultToolLabel(f.tool)}</span><span>{f.mode} × {f.remaining} left</span></li>
				{:else}
					<li class="muted">No faults armed.</li>
				{/each}
			</ul>
		{:else if faults.value.state === 'unavailable'}
			<Unavailable reason={faults.value.reason} retry={() => faults.refresh()} />
		{/if}
	</Card>
</div>

<Card title="Reset all demo data">
	<div class="row">
		<label class="field">
			<span class="label">Seed</span>
			<input class="input" type="number" bind:value={seed} disabled={!admin} title={admin ? '' : denied} />
		</label>
		{#if !confirmReset}
			<button
				class="btn danger"
				type="button"
				disabled={!admin}
				title={admin ? '' : denied}
				onclick={() => (confirmReset = true)}>Reset…</button
			>
		{:else}
			<span>Cases in flight will end as failed. Reset now?</span>
			<button
				class="btn danger"
				type="button"
				onclick={() =>
					guard(() => api().demoReset(seed), `Demo data reset with seed ${seed}.`).then((ok) => {
						if (!ok) return;
						confirmReset = false;
						faults.refresh();
					})}>Yes, reset</button
			>
			<button class="btn" type="button" onclick={() => (confirmReset = false)}>Cancel</button>
		{/if}
	</div>
</Card>

<style>
	.dm-tile {
		display: grid;
		gap: 4px;
		padding: 14px 16px;
		text-decoration: none;
		color: inherit;
	}
	.dm-tile:hover {
		border-color: var(--accent);
	}
</style>
