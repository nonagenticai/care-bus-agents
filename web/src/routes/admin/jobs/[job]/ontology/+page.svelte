<script lang="ts">
	import { api, guard } from '$lib/app.svelte';
	import { ApiError } from '$lib/api/client';
	import { session } from '$lib/auth/session.svelte';
	import { Resource } from '$lib/load.svelte';
	import type {
		Attribute,
		Cardinality,
		ContextSpace,
		EntityInput,
		OntologyEntity,
		OntologyRelation,
		Paged,
		Scope,
	} from '$lib/api/types';
	import Freshness from '$lib/ui/Freshness.svelte';
	import Card from '$lib/ui/Card.svelte';
	import Chip from '$lib/ui/Chip.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Seg from '$lib/ui/Seg.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import OntologyGraph from '$lib/ontology/OntologyGraph.svelte';
	import {
		CARDINALITIES,
		CARDINALITY_LABEL,
		SCOPE_LABEL,
		distinct,
		entityFormErrors,
		heading,
		layout,
		relationFormErrors,
		relationsOf,
		title,
		visibleEntities,
		type Filter,
	} from '$lib/ontology/layout';

	const spaces = new Resource<Paged<ContextSpace>>(() => api().contextSpaces());
	// include_hidden: hidden entities are absent from the effective list; this page lists them under "Show hidden".
	const entities = new Resource<Paged<OntologyEntity>>(() => api().entities({ includeHidden: true }));
	const relations = new Resource<Paged<OntologyRelation>>(() => api().relations());
	spaces.refresh();
	entities.refresh();
	relations.refresh();
	const fresh = {
		get refreshing() {
			return entities.refreshing || relations.refreshing || spaces.refreshing;
		},
		get updatedAt() {
			const t = [entities.updatedAt, relations.updatedAt, spaces.updatedAt].filter((d): d is Date => !!d);
			return t.length ? new Date(Math.min(...t.map((d) => d.getTime()))) : null;
		},
		refresh: async () => {
			await Promise.all([spaces.refresh(), entities.refresh(), relations.refresh()]);
		},
	};

	const canWrite = $derived(session.can('tenant_admin'));
	let view = $state('graph');
	let filter = $state<Filter>({ industry: 'all', department: 'all', space: 'all', showHidden: false });

	const allEntities = $derived(entities.value.state === 'ok' ? entities.value.data.items : []);
	const allRelations = $derived(relations.value.state === 'ok' ? relations.value.data.items : []);
	const allSpaces = $derived(spaces.value.state === 'ok' ? spaces.value.data.items : []);
	const industries = $derived(distinct(allEntities, 'industry'));
	const departments = $derived(distinct(allEntities, 'department', filter.industry));
	const shown = $derived(visibleEntities(allEntities, filter, allSpaces));
	const graph = $derived(layout(shown, allRelations));
	const head = $derived(heading(filter));
	const hiddenCount = $derived(allEntities.filter((e) => e.hidden).length);

	function pickIndustry(v: string) {
		filter.industry = v;
		if (v === 'all' || !distinct(allEntities, 'department', v).includes(filter.department))
			filter.department = 'all';
	}

	// ---- entity detail ----
	let open = $state(false);
	let sel = $state<OntologyEntity | null>(null);
	let synonym = $state('');
	const selRel = $derived(sel ? relationsOf(sel.name, allRelations) : { out: [], in: [] });
	const labelOf = (name: string) => allEntities.find((e) => e.name === name)?.label ?? name;

	function select(e: OntologyEntity) {
		sel = e;
		synonym = '';
		confirmKey = null;
		relError = null;
		open = true;
	}

	/** Turns a failed write into the inline message the form shows. */
	function explain(e: unknown, what: string): string {
		if (e instanceof ApiError && e.status === 403) return `You need the tenant_admin role to ${what}.`;
		if (e instanceof ApiError && e.status === 0) return 'carebus-api is unreachable. Nothing was saved.';
		if (e instanceof ApiError) return `${e.message} (${e.code})`;
		return (e as Error).message;
	}

	// ---- delete a relation (from the entity drawer) ----
	const relKey = (r: OntologyRelation) => `${r.subject}|${r.predicate}|${r.object}`;
	let confirmKey = $state<string | null>(null);
	let relError = $state<string | null>(null);
	let deleting = $state(false);

	async function deleteRelation(r: OntologyRelation) {
		deleting = true;
		relError = null;
		try {
			await api().deleteRelation(r);
			confirmKey = null;
			await relations.refresh();
		} catch (e) {
			relError = explain(e, 'delete relations');
		} finally {
			deleting = false;
		}
	}

	// ---- add relation ----
	type RelDraft = { subject: string; predicate: string; object: string; cardinality: Cardinality | '' };
	let addingRel = $state(false);
	let relDraft = $state<RelDraft>({ subject: '', predicate: '', object: '', cardinality: '1:n' });
	let relStart = $state('');
	const relDirty = $derived(addingRel && JSON.stringify(relDraft) !== relStart);
	let relFormErrors = $state<string[]>([]);
	let relApiError = $state<string | null>(null);
	let savingRel = $state(false);
	const pickable = $derived([...allEntities].sort((a, b) => a.label.localeCompare(b.label)));

	function startAddRelation(subject = '') {
		relDraft = { subject, predicate: '', object: '', cardinality: '1:n' };
		relStart = JSON.stringify(relDraft);
		relFormErrors = [];
		relApiError = null;
		addingRel = true;
	}

	async function submitRelation() {
		const input = { ...relDraft, predicate: relDraft.predicate.trim() };
		relFormErrors = relationFormErrors(input, allEntities, allRelations);
		relApiError = null;
		if (relFormErrors.length) return;
		savingRel = true;
		try {
			await api().createRelation({ ...input, cardinality: input.cardinality as Cardinality });
			addingRel = false;
			await relations.refresh();
		} catch (e) {
			relApiError = explain(e, 'add relations');
		} finally {
			savingRel = false;
		}
	}

	function addSynonym() {
		if (!sel || !synonym.trim()) return;
		const name = sel.name;
		const term = synonym.trim();
		// PUT takes the full EntityInput (plan 05 routes::ontology::update); the overlay fields are not part of it.
		const { hidden: _hidden, origin: _origin, overridden: _overridden, ...input } = sel;
		const next: EntityInput = { ...input, synonyms: [...sel.synonyms, term] };
		guard(() => api().saveEntity(next), `Added "${term}" to ${sel.label}.`).then(async (ok) => {
			if (!ok) return;
			synonym = '';
			await entities.refresh();
			if (entities.value.state === 'ok') sel = entities.value.data.items.find((e) => e.name === name) ?? sel;
		});
	}

	// ---- add entity ----
	type Draft = {
		name: string;
		label: string;
		scope: Scope;
		industry: string;
		department: string;
		synonyms: string;
		attributes: (Omit<Attribute, 'synonyms'> & { synonyms: string })[];
	};
	const blankAttr = () => ({ name: '', data_type: 'string', pii: false, synonyms: '' });
	const blank = (): Draft => ({
		name: '',
		label: '',
		scope: filter.department !== 'all' ? 'industry_dept' : filter.industry !== 'all' ? 'industry' : 'generic',
		industry: filter.industry === 'all' ? (industries[0] ?? '') : filter.industry,
		department: filter.department === 'all' ? '' : filter.department,
		synonyms: '',
		attributes: [blankAttr()],
	});
	const TYPES = ['string', 'text', 'enum', 'date', 'datetime', 'money', 'number', 'boolean'];
	let adding = $state(false);
	let draft = $state<Draft>(blank());
	let draftStart = $state('');
	const entityDirty = $derived(adding && JSON.stringify(draft) !== draftStart);
	let formErrors = $state<string[]>([]);
	let apiError = $state<string | null>(null);
	let saving = $state(false);
	const list = (s: string) =>
		s
			.split(',')
			.map((x) => x.trim())
			.filter(Boolean);

	function startAdd() {
		draft = blank();
		draftStart = JSON.stringify(draft);
		formErrors = [];
		apiError = null;
		adding = true;
	}

	function toInput(d: Draft): EntityInput {
		return {
			name: d.name.trim(),
			label: d.label.trim(),
			scope: d.scope,
			industry: d.scope === 'generic' ? null : d.industry.trim() || null,
			department: d.scope === 'industry_dept' ? d.department.trim() || null : null,
			synonyms: list(d.synonyms),
			attributes: d.attributes.map((a) => ({
				name: a.name.trim(),
				data_type: a.data_type.trim(),
				pii: a.pii,
				synonyms: list(a.synonyms),
			})),
		};
	}

	async function submitEntity() {
		const input = toInput(draft);
		formErrors = entityFormErrors(input);
		apiError = null;
		if (formErrors.length) return;
		saving = true;
		try {
			const created = await api().createEntity(input);
			adding = false;
			await entities.refresh();
			const fresh =
				entities.value.state === 'ok' ? entities.value.data.items.find((e) => e.name === input.name) : null;
			select(fresh ?? created);
		} catch (e) {
			apiError = explain(e, 'add entities');
		} finally {
			saving = false;
		}
	}
</script>

<header class="page-head">
	<div>
		<h1>Ontology</h1>
		<p>Entities, attributes and relations per layer. Context spaces derive the bus topics.</p>
	</div>
	<div class="actions">
		<Freshness resource={fresh} />
		{#if canWrite}
			<button type="button" class="btn primary" onclick={startAdd}>Add entity</button>
			<button type="button" class="btn" onclick={() => startAddRelation()}>Add relation</button>
		{/if}
	</div>
</header>

<section class="scopebar card">
	<div class="scope-title">
		<h2>{head.text}</h2>
		<span class="level {head.level}">{head.level === 'all' ? 'All layers' : SCOPE_LABEL[head.level]}</span>
		<span class="muted counts">
			<b>{graph.nodes.length}</b> entities · <b>{graph.edges.length}</b> relations
		</span>
	</div>
	<div class="filters">
		<div class="frow" role="group" aria-label="Industry">
			<span class="flabel">Industry</span>
			<button
				type="button"
				class="chip"
				class:accent={filter.industry === 'all'}
				aria-pressed={filter.industry === 'all'}
				onclick={() => pickIndustry('all')}>All</button
			>
			{#each industries as ind}
				<button
					type="button"
					class="chip"
					class:accent={filter.industry === ind}
					aria-pressed={filter.industry === ind}
					onclick={() => pickIndustry(ind)}>{title(ind)}</button
				>
			{/each}
		</div>
		<div class="frow" role="group" aria-label="Department">
			<span class="flabel">Department</span>
			<button
				type="button"
				class="chip"
				class:accent={filter.department === 'all'}
				aria-pressed={filter.department === 'all'}
				onclick={() => (filter.department = 'all')}>All</button
			>
			{#each departments as d}
				<button
					type="button"
					class="chip"
					class:accent={filter.department === d}
					aria-pressed={filter.department === d}
					onclick={() => (filter.department = d)}>{title(d)}</button
				>
			{/each}
		</div>
		<div class="frow">
			<label class="flabel" for="onto-space">Context space</label>
			<select id="onto-space" class="input sm" bind:value={filter.space}>
				<option value="all">All spaces</option>
				{#each allSpaces as s}<option value={s.key}>{s.label} ({s.area})</option>{/each}
			</select>
			{#if hiddenCount > 0}
				<label class="check"
					><input type="checkbox" bind:checked={filter.showHidden} /> Show hidden ({hiddenCount})</label
				>
			{/if}
			<span class="spacer"></span>
			<Seg
				items={[
					['graph', 'Graph'],
					['table', 'Relations list'],
				]}
				bind:active={view}
			/>
		</div>
	</div>
</section>

{#if entities.value.state === 'unavailable'}
	<Unavailable reason={entities.value.reason} retry={() => entities.refresh()} />
{:else if entities.value.state === 'empty'}
	<div class="empty">No entities yet. Import a seed or add one.</div>
{:else if entities.value.state === 'loading'}
	<p class="muted">Loading…</p>
{:else if view === 'graph'}
	{#if relations.value.state === 'unavailable'}
		<Unavailable reason={relations.value.reason} retry={() => relations.refresh()} />
	{/if}
	{#if shown.length === 0}
		<div class="empty">No entities match these filters.</div>
	{:else}
		<div class="canvas card">
			<OntologyGraph
				entities={shown}
				relations={allRelations}
				selected={open ? sel?.name : null}
				onselect={select}
			/>
		</div>
	{/if}
{:else}
	<Card title="Relations" cls="flush">
		{#if relations.value.state === 'ok'}
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead><tr><th>Subject</th><th>Predicate</th><th>Object</th><th>Cardinality</th></tr></thead>
					<tbody>
						{#each graph.edges as e (e.id)}
							{@const r = e.data!.relation}
							<tr>
								<td>{labelOf(r.subject)}</td><td class="mono">{r.predicate}</td><td>{labelOf(r.object)}</td
								><td class="mono">{r.cardinality}</td>
							</tr>
						{:else}
							<tr><td colspan="4" class="muted">No relations between the entities in this filter.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else if relations.value.state === 'unavailable'}
			<Unavailable reason={relations.value.reason} />
		{:else if relations.value.state === 'empty'}
			<div class="empty">No relations.</div>
		{:else}
			<p class="muted">Loading…</p>
		{/if}
	</Card>

	<Card
		title="Context spaces → bus topics"
		sub="Each department gets its own streams; the topic prefix is the space key"
		cls="flush"
	>
		{#if spaces.value.state === 'ok'}
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead
						><tr
							><th>Key</th><th>Topic prefix</th><th>Entities</th><th>Required fields</th><th>Streams</th><th
								>Procedure</th
							></tr
						></thead
					>
					<tbody>
						{#each spaces.value.data.items as s}
							<tr>
								<td class="mono">{s.key}</td>
								<td class="mono">{s.topic_prefix}</td>
								<td>{s.entities.map(labelOf).join(', ')}</td>
								<td>{s.required.join(', ')}</td>
								<td>
									<div class="chips">
										<Chip text={s.streams.customer} variant="cust" />
										{#each s.streams.worker as st}<Chip text={st} variant="work" />{/each}
										<Chip text={s.streams.worker_results} variant="work" />
									</div>
								</td>
								<td class="mono">{s.procedure}</td>
							</tr>
						{/each}
					</tbody>
				</table>
			</div>
		{:else if spaces.value.state === 'unavailable'}
			<Unavailable reason={spaces.value.reason} />
		{:else if spaces.value.state === 'empty'}
			<div class="empty">No context spaces.</div>
		{:else}
			<p class="muted">Loading…</p>
		{/if}
	</Card>
{/if}

<Drawer bind:open title={sel?.label ?? ''} wide>
	{#snippet sub()}
		{#if sel}
			<span class="level {sel.scope}">{SCOPE_LABEL[sel.scope] ?? sel.scope}</span>
			<span class="mono"
				>{sel.name}{sel.industry ? ` · ${sel.industry}` : ''}{sel.department
					? ` · ${sel.department}`
					: ''}</span
			>
			{#if sel.hidden}<Chip text="hidden" />{/if}
		{/if}
	{/snippet}
	{#if sel}
		<section>
			<h3>Synonyms</h3>
			<div class="chips">
				{#each sel.synonyms as s}<Chip text={s} />{:else}<span class="muted">None.</span>{/each}
			</div>
			{#if canWrite}
				<form
					class="row syn"
					onsubmit={(e) => {
						e.preventDefault();
						addSynonym();
					}}
				>
					<input class="input" bind:value={synonym} placeholder="Add a synonym" aria-label="New synonym" />
					<button class="btn sm" type="submit">Add</button>
				</form>
			{/if}
		</section>
		<section>
			<h3>Attributes</h3>
			<div class="tbl-wrap">
				<table class="tbl dense">
					<thead><tr><th>Name</th><th>Type</th><th>Personal data</th><th>Synonyms</th></tr></thead>
					<tbody>
						{#each sel.attributes as a (a.name)}
							<tr>
								<td class="mono">{a.name}</td>
								<td class="mono">{a.data_type}</td>
								<td
									>{#if a.pii}<Chip text="personal data" variant="err" />{:else}<span class="muted">no</span
										>{/if}</td
								>
								<td>{a.synonyms.join(', ')}</td>
							</tr>
						{:else}
							<tr><td colspan="4" class="muted">No attributes.</td></tr>
						{/each}
					</tbody>
				</table>
			</div>
		</section>
		{#if relError}
			<div class="callout err" role="alert"><strong>Not deleted</strong><span>{relError}</span></div>
		{/if}
		<section>
			<div class="sec-head">
				<h3>Relations out ({selRel.out.length})</h3>
				{#if canWrite}
					<button type="button" class="btn sm" onclick={() => startAddRelation(sel!.name)}
						>Add relation</button
					>
				{/if}
			</div>
			{#each selRel.out as r (relKey(r))}
				<div class="rel">
					<span class="mono">{r.predicate}</span> →
					<button
						type="button"
						class="link"
						onclick={() => (sel = allEntities.find((e) => e.name === r.object) ?? sel)}
						>{labelOf(r.object)}</button
					>
					<span class="mono muted">{r.cardinality}</span>
					{@render relDelete(r)}
				</div>
			{:else}
				<p class="muted">None.</p>
			{/each}
		</section>
		<section>
			<h3>Relations in ({selRel.in.length})</h3>
			{#each selRel.in as r (relKey(r))}
				<div class="rel">
					<button
						type="button"
						class="link"
						onclick={() => (sel = allEntities.find((e) => e.name === r.subject) ?? sel)}
						>{labelOf(r.subject)}</button
					>
					<span class="mono">{r.predicate}</span> → this
					<span class="mono muted">{r.cardinality}</span>
					{@render relDelete(r)}
				</div>
			{:else}
				<p class="muted">None.</p>
			{/each}
		</section>
	{/if}
</Drawer>

{#snippet relDelete(r: OntologyRelation)}
	{#if canWrite}
		<span class="spacer"></span>
		{#if confirmKey === relKey(r)}
			<span class="confirm" role="group" aria-label="Confirm delete">
				<span class="muted">Delete {r.subject} {r.predicate} {r.object}?</span>
				<button type="button" class="btn sm danger" disabled={deleting} onclick={() => deleteRelation(r)}
					>{deleting ? 'Deleting…' : 'Delete'}</button
				>
				<button type="button" class="btn sm" onclick={() => (confirmKey = null)}>Keep</button>
			</span>
		{:else}
			<button
				type="button"
				class="btn sm"
				aria-label="Delete relation {r.subject} {r.predicate} {r.object}"
				onclick={() => {
					confirmKey = relKey(r);
					relError = null;
				}}>Delete</button
			>
		{/if}
	{/if}
{/snippet}

<Drawer bind:open={addingRel} title="Add relation" dirty={relDirty}>
	<form
		class="stack"
		novalidate
		onsubmit={(e) => {
			e.preventDefault();
			submitRelation();
		}}
	>
		<div class="field">
			<label for="rel-subject">Subject</label>
			<select id="rel-subject" class="input" bind:value={relDraft.subject}>
				<option value="">Pick an entity…</option>
				{#each pickable as en (en.name)}<option value={en.name}>{en.label} ({en.name})</option>{/each}
			</select>
		</div>
		<div class="field">
			<label for="rel-predicate">Predicate</label>
			<input
				id="rel-predicate"
				class="input mono"
				bind:value={relDraft.predicate}
				placeholder="e.g. places, paidBy"
				autocomplete="off"
			/>
		</div>
		<div class="field">
			<label for="rel-object">Object</label>
			<select id="rel-object" class="input" bind:value={relDraft.object}>
				<option value="">Pick an entity…</option>
				{#each pickable as en (en.name)}<option value={en.name}>{en.label} ({en.name})</option>{/each}
			</select>
		</div>
		<div class="field">
			<label for="rel-card">Cardinality</label>
			<select id="rel-card" class="input" bind:value={relDraft.cardinality}>
				{#each CARDINALITIES as c}<option value={c}>{c} · {CARDINALITY_LABEL[c]}</option>{/each}
			</select>
		</div>
		{#if relDraft.subject && relDraft.object && relDraft.predicate.trim()}
			<p class="muted preview">
				Reads as: <b>{labelOf(relDraft.subject)}</b>
				<span class="mono">{relDraft.predicate.trim()}</span>
				<b>{labelOf(relDraft.object)}</b>
			</p>
		{/if}
		{#if relFormErrors.length}
			<div class="callout err" role="alert">
				<strong>Fix these first</strong>
				<span>{relFormErrors.join(' ')}</span>
			</div>
		{/if}
		{#if relApiError}
			<div class="callout err" role="alert"><strong>Not saved</strong><span>{relApiError}</span></div>
		{/if}
		<div class="row">
			<button type="submit" class="btn primary" disabled={savingRel}
				>{savingRel ? 'Saving…' : 'Create relation'}</button
			>
			<button type="button" class="btn" onclick={() => (addingRel = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<Drawer bind:open={adding} title="Add entity" wide dirty={entityDirty}>
	<form
		class="stack"
		novalidate
		onsubmit={(e) => {
			e.preventDefault();
			submitEntity();
		}}
	>
		<div class="grid g2">
			<div class="field">
				<label for="ent-name">Name</label>
				<input id="ent-name" class="input mono" bind:value={draft.name} placeholder="e.g. Invoice" />
			</div>
			<div class="field">
				<label for="ent-label">Label</label>
				<input id="ent-label" class="input" bind:value={draft.label} placeholder="e.g. Invoice" />
			</div>
			<div class="field">
				<label for="ent-scope">Scope</label>
				<select id="ent-scope" class="input" bind:value={draft.scope}>
					<option value="generic">Generic</option>
					<option value="industry">Industry</option>
					<option value="industry_dept">Specialized (industry × department)</option>
				</select>
			</div>
			<div class="field">
				<label for="ent-syn">Synonyms (comma separated)</label>
				<input id="ent-syn" class="input" bind:value={draft.synonyms} placeholder="bill, factuur" />
			</div>
			{#if draft.scope !== 'generic'}
				<div class="field">
					<label for="ent-ind">Industry</label>
					<input id="ent-ind" class="input" list="onto-industries" bind:value={draft.industry} />
					<datalist id="onto-industries"
						>{#each industries as i}<option value={i}></option>{/each}</datalist
					>
				</div>
			{/if}
			{#if draft.scope === 'industry_dept'}
				<div class="field">
					<label for="ent-dep">Department</label>
					<input id="ent-dep" class="input" list="onto-departments" bind:value={draft.department} />
					<datalist id="onto-departments"
						>{#each distinct(allEntities, 'department') as d}<option value={d}></option>{/each}</datalist
					>
				</div>
			{/if}
		</div>

		<div>
			<h3>Attributes</h3>
			<div class="tbl-wrap">
				<table class="tbl dense attrs">
					<thead><tr><th>Name</th><th>Type</th><th>Personal data</th><th>Synonyms</th><th></th></tr></thead>
					<tbody>
						{#each draft.attributes as a, i}
							<tr>
								<td><input class="input mono" aria-label="Attribute {i + 1} name" bind:value={a.name} /></td>
								<td>
									<input
										class="input mono"
										aria-label="Attribute {i + 1} type"
										list="onto-types"
										bind:value={a.data_type}
									/>
								</td>
								<td class="c"
									><input
										type="checkbox"
										aria-label="Attribute {i + 1} is personal data"
										bind:checked={a.pii}
									/></td
								>
								<td
									><input class="input" aria-label="Attribute {i + 1} synonyms" bind:value={a.synonyms} /></td
								>
								<td>
									<button
										type="button"
										class="btn sm"
										aria-label="Remove attribute {i + 1}"
										onclick={() => draft.attributes.splice(i, 1)}>Remove</button
									>
								</td>
							</tr>
						{:else}
							<tr><td colspan="5" class="muted">No attributes.</td></tr>
						{/each}
					</tbody>
				</table>
				<datalist id="onto-types"
					>{#each TYPES as t}<option value={t}></option>{/each}</datalist
				>
			</div>
			<button type="button" class="btn sm add-attr" onclick={() => draft.attributes.push(blankAttr())}
				>Add attribute</button
			>
		</div>

		{#if formErrors.length}
			<div class="callout err" role="alert">
				<strong>Fix these first</strong>
				<span>{formErrors.join(' ')}</span>
			</div>
		{/if}
		{#if apiError}
			<div class="callout err" role="alert"><strong>Not saved</strong><span>{apiError}</span></div>
		{/if}
		<div class="row">
			<button type="submit" class="btn primary" disabled={saving}
				>{saving ? 'Saving…' : 'Create entity'}</button
			>
			<button type="button" class="btn" onclick={() => (adding = false)}>Cancel</button>
		</div>
	</form>
</Drawer>

<style>
	.actions {
		display: flex;
		gap: 10px;
		align-items: flex-start;
	}
	.sec-head {
		display: flex;
		align-items: baseline;
		justify-content: space-between;
		gap: 8px;
	}
	.confirm {
		display: inline-flex;
		gap: 6px;
		align-items: baseline;
		flex-wrap: wrap;
	}
	.preview {
		margin: 0;
		font-size: 13px;
	}
	.scopebar {
		display: grid;
		gap: 10px;
		padding: 12px 14px;
	}
	.scope-title {
		display: flex;
		align-items: center;
		gap: 10px;
		flex-wrap: wrap;
	}
	.scope-title h2 {
		margin: 0;
		font: 600 18px/1.2 var(--cond);
	}
	.counts b {
		color: var(--fg);
		font-variant-numeric: tabular-nums;
	}
	.level {
		--lv: var(--accent);
		font: 600 10px/1.4 var(--sans);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		padding: 2px 7px;
		border-radius: 999px;
		color: var(--lv);
		background: color-mix(in srgb, var(--lv) 14%, transparent);
	}
	.level.generic {
		--lv: var(--cust);
	}
	.level.industry {
		--lv: var(--tool);
	}
	.level.industry_dept {
		--lv: var(--work);
	}
	.filters {
		display: grid;
		gap: 8px;
	}
	.frow {
		display: flex;
		align-items: center;
		gap: 6px;
		flex-wrap: wrap;
	}
	.flabel {
		min-width: 92px;
		font: 600 11px/1.2 var(--cond);
		letter-spacing: 0.07em;
		text-transform: uppercase;
		color: var(--muted);
	}
	button.chip {
		cursor: pointer;
		border: 1px solid var(--line);
		background: var(--panel);
		color: var(--fg);
	}
	button.chip.accent {
		border-color: color-mix(in srgb, var(--accent) 40%, var(--line));
		background: color-mix(in srgb, var(--accent) 13%, transparent);
		color: var(--accent);
	}
	.input.sm {
		width: auto;
		padding: 4px 8px;
		font-size: 12.5px;
	}
	.check {
		display: inline-flex;
		gap: 5px;
		align-items: center;
		font-size: 12.5px;
		color: var(--muted);
	}
	.spacer {
		flex: 1;
	}
	.canvas {
		padding: 0;
		overflow: hidden;
		height: 620px;
	}
	.syn {
		margin-top: 8px;
	}
	.syn .input {
		flex: 1;
	}
	.rel {
		display: flex;
		flex-wrap: wrap;
		gap: 6px;
		align-items: baseline;
		padding: 3px 0;
		font-size: 13px;
	}
	.link {
		all: unset;
		cursor: pointer;
		color: var(--accent);
	}
	.link:hover {
		text-decoration: underline;
	}
	.link:focus-visible {
		outline: 2px solid var(--accent);
		outline-offset: 2px;
	}
	.attrs .input {
		padding: 4px 7px;
	}
	.add-attr {
		margin-top: 8px;
	}
	h3 {
		margin: 0 0 8px;
		font: 600 13px/1.3 var(--cond);
		letter-spacing: 0.06em;
		text-transform: uppercase;
		color: var(--muted);
	}
</style>
