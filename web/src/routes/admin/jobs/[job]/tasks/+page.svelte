<script lang="ts">
	// Tasks: the staff inbox of long-running MCP work. Open tasks are read again every few seconds while any
	// exists (a failed read backs off, $lib/poll). Staff answer a task's questions and cancel tasks; agents never do.
	import { onDestroy } from 'svelte';
	import { api } from '$lib/app.svelte';
	import { session } from '$lib/auth/session.svelte';
	import { afterPollError } from '$lib/poll';
	import { TASKS_POLL_MS, isTaskTerminal, type TaskListState, type TaskView } from '$lib/api/tasks';
	import Card from '$lib/ui/Card.svelte';
	import Callout from '$lib/ui/Callout.svelte';
	import Unavailable from '$lib/ui/Unavailable.svelte';
	import TaskCard from './TaskCard.svelte';

	const canWrite = $derived(session.can('tenant_user'));

	let view = $state<TaskListState>('open');
	let tasks = $state<TaskView[]>([]);
	let phase = $state<'loading' | 'ok' | 'unavailable'>('loading');
	let loadError = $state('');
	let pollErr = $state('');
	let failures = 0;
	let gen = 0;
	let timer: ReturnType<typeof setTimeout> | undefined;

	async function read(g: number) {
		let items: TaskView[];
		try {
			items = (await api().mcpTasks(view)).items;
		} catch (e) {
			if (g !== gen) return;
			if (phase !== 'ok') {
				loadError = e instanceof Error ? e.message : String(e);
				phase = 'unavailable';
				return;
			}
			failures += 1;
			const step = afterPollError(e, failures, TASKS_POLL_MS);
			if (step.kind === 'retry') timer = setTimeout(() => read(g), step.delay);
			else
				pollErr =
					step.kind === 'gone'
						? 'The task list is no longer available.'
						: step.kind === 'auth'
							? step.message
							: `Stopped refreshing after ${failures} failed reads: ${step.message}`;
			return;
		}
		if (g !== gen) return;
		failures = 0;
		pollErr = '';
		tasks = items;
		phase = 'ok';
		if (items.some((t) => !isTaskTerminal(t.status))) timer = setTimeout(() => read(g), TASKS_POLL_MS);
	}

	function reload() {
		clearTimeout(timer);
		const g = ++gen;
		failures = 0;
		void read(g);
	}

	$effect(() => {
		void view;
		reload();
		return () => {
			gen += 1;
			clearTimeout(timer);
		};
	});
	onDestroy(() => {
		gen += 1;
		clearTimeout(timer);
	});

	function changed(t: TaskView) {
		const i = tasks.findIndex((x) => x.task_id === t.task_id);
		if (i >= 0) tasks[i] = t;
		if (view === 'open' && isTaskTerminal(t.status)) tasks = tasks.filter((x) => x.task_id !== t.task_id);
		clearTimeout(timer);
		if (tasks.some((x) => !isTaskTerminal(x.status))) {
			const g = gen;
			timer = setTimeout(() => read(g), TASKS_POLL_MS);
		}
	}
</script>

<header class="page-head">
	<div>
		<h1>Tasks</h1>
		<p>
			Long-running work started by agents. When a task needs a decision, a person answers here; agents never
			answer.
		</p>
	</div>
</header>

{#if !canWrite}
	<Callout kind="info" title="Read only">Answering or cancelling a task needs a role in this tenant.</Callout>
{/if}
{#if pollErr}
	<Callout kind="err" title="Not refreshing"
		>{pollErr} <button class="btn" type="button" onclick={reload}>Retry</button></Callout
	>
{/if}

<Card title="Tasks" cls="flush">
	{#snippet actions()}
		<label class="field">
			<span class="label">Show</span>
			<select class="input" bind:value={view} aria-label="Task filter">
				<option value="open">Open</option>
				<option value="all">All</option>
			</select>
		</label>
	{/snippet}
	{#if phase === 'loading'}
		<p class="muted">Loading…</p>
	{:else if phase === 'unavailable'}
		<Unavailable reason={loadError} retry={reload} />
	{:else if tasks.length === 0}
		<div class="empty">{view === 'open' ? 'No open tasks.' : 'No tasks yet.'}</div>
	{:else}
		{#each tasks as t (t.task_id)}
			<TaskCard task={t} {canWrite} onchange={changed} onrefresh={reload} />
		{/each}
	{/if}
</Card>
