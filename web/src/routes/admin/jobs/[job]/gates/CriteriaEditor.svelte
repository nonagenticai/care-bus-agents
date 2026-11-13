<script lang="ts">
	import { untrack } from 'svelte';
	import { choiceOptions, scoreLevels, type GateCriteria, type GateType } from '$lib/api/gates';

	// Mounted per question and per type (the page keys it), so the rows are read from `criteria` once and every
	// edit writes the whole criteria back.
	let {
		type,
		criteria = $bindable(),
		disabled = false,
		error = '',
		invalid = $bindable(''),
	}: {
		type: GateType;
		criteria: GateCriteria;
		disabled?: boolean;
		error?: string;
		/** A row problem the criteria map cannot carry (a blank or repeated option name); '' when none. */
		invalid?: string;
	} = $props();

	const [t0, init] = untrack(() => [type, $state.snapshot(criteria) as GateCriteria] as const);
	const n0 = !Array.isArray(init) && init ? (init as Record<string, string>) : {};
	let noulTrue = $state(t0 === 'noul' ? (n0.true ?? '') : '');
	let noulFalse = $state(t0 === 'noul' ? (n0.false ?? '') : '');
	let options = $state(t0 === 'choice' ? choiceOptions(init).map(([name, desc]) => ({ name, desc })) : []);
	let levels = $state(t0 === 'score' ? scoreLevels(init) : []);

	function emit() {
		if (type === 'noul') criteria = { true: noulTrue, false: noulFalse };
		else if (type === 'choice') {
			const names = options.map((r) => r.name.trim());
			invalid = names.some((n) => !n)
				? 'Every option needs a name.'
				: new Set(names).size !== names.length
					? 'Option names must be distinct.'
					: '';
			criteria = Object.fromEntries(options.map((r) => [r.name.trim(), r.desc]));
		} else criteria = [...levels];
	}
	function addOption() {
		options.push({ name: '', desc: '' });
		emit();
	}
	function removeOption(i: number) {
		options.splice(i, 1);
		emit();
	}
	function addLevel() {
		levels.push('');
		emit();
	}
	function removeLevel(i: number) {
		levels.splice(i, 1);
		emit();
	}
	function moveLevel(i: number, d: -1 | 1) {
		const j = i + d;
		if (j < 0 || j >= levels.length) return;
		[levels[i], levels[j]] = [levels[j]!, levels[i]!];
		emit();
	}
</script>

<fieldset class="crit" {disabled} aria-describedby={invalid || error ? 'gate-err-criteria' : undefined}>
	<legend class="label">Criteria</legend>
	{#if type === 'noul'}
		<label class="field"
			><span class="label">True when</span><textarea
				class="input"
				rows="2"
				bind:value={noulTrue}
				oninput={emit}
			></textarea></label
		>
		<label class="field"
			><span class="label">False when</span><textarea
				class="input"
				rows="2"
				bind:value={noulFalse}
				oninput={emit}
			></textarea></label
		>
	{:else if type === 'choice'}
		<p class="muted">2–255 options. laya picks one and reports a probability per option.</p>
		{#each options as o, i (i)}
			<div class="row">
				<input
					class="input mono name"
					aria-label="Option {i + 1} name"
					placeholder="option"
					bind:value={o.name}
					oninput={emit}
				/>
				<input
					class="input"
					aria-label="Option {i + 1} description"
					placeholder="what this option means"
					bind:value={o.desc}
					oninput={emit}
				/>
				<button
					class="btn sm"
					type="button"
					aria-label="Remove option {i + 1}"
					disabled={options.length <= 2}
					onclick={() => removeOption(i)}>Remove</button
				>
			</div>
		{/each}
		<button class="btn sm" type="button" disabled={options.length >= 255} onclick={addOption}
			>Add option</button
		>
	{:else}
		<p class="muted">2–10 ordered levels, lowest first. The score is the level's index.</p>
		{#each levels as _, i (i)}
			<div class="row">
				<span class="mono idx">{i}</span>
				<input
					class="input"
					aria-label="Level {i}"
					placeholder="level label"
					bind:value={levels[i]}
					oninput={emit}
				/>
				<button
					class="btn sm"
					type="button"
					aria-label="Move level {i} up"
					disabled={i === 0}
					onclick={() => moveLevel(i, -1)}>↑</button
				>
				<button
					class="btn sm"
					type="button"
					aria-label="Move level {i} down"
					disabled={i === levels.length - 1}
					onclick={() => moveLevel(i, 1)}>↓</button
				>
				<button
					class="btn sm"
					type="button"
					aria-label="Remove level {i}"
					disabled={levels.length <= 2}
					onclick={() => removeLevel(i)}>Remove</button
				>
			</div>
		{/each}
		<button class="btn sm" type="button" disabled={levels.length >= 10} onclick={addLevel}>Add level</button>
	{/if}
	{#if invalid || error}<span class="field-err" id="gate-err-criteria">{invalid || error}</span>{/if}
</fieldset>

<style>
	.crit {
		border: 1px solid var(--line-2);
		border-radius: var(--r);
		padding: 10px 12px;
		margin: 0;
		min-width: 0;
		display: grid;
		gap: 8px;
	}
	.row {
		display: flex;
		gap: 6px;
		align-items: center;
		flex-wrap: wrap;
	}
	.row .input {
		flex: 1 1 160px;
		min-width: 0;
	}
	.row .name {
		flex: 0 1 160px;
	}
	.idx {
		width: 1.5em;
		text-align: right;
		color: var(--muted);
	}
	.field-err {
		color: var(--err);
		font-size: 12px;
	}
</style>
