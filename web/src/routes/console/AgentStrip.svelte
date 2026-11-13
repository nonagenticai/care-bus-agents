<script lang="ts">
	import type { AgentCard } from '$lib/console/bus';
	let {
		cards,
		selected,
		onselect,
	}: { cards: AgentCard[]; selected: string | null; onselect: (id: string) => void } = $props();
	const GROUPS = [
		{ kind: 'front', label: 'Front', cls: '' },
		{ kind: 'coordination', label: 'Coordination', cls: '' },
		{ kind: 'worker', label: 'Worker agents · one per tool', cls: 'wk' },
		{ kind: 'bridge', label: 'MCP bridge', cls: '' },
	] as const;
</script>

<section class="panel agents" aria-label="Agents">
	{#each GROUPS as g (g.kind)}
		<div class="group {g.cls}">
			<span class="label">{g.label}</span>
			<div class:workers={g.kind === 'worker'}>
				{#each cards.filter((c) => c.kind === g.kind) as a (a.id)}
					<button
						type="button"
						class="agent"
						class:small={g.kind === 'worker'}
						class:sel={selected === a.id}
						data-s={a.status}
						data-agent={a.id}
						title={a.line}
						onclick={() => onselect(a.id)}
					>
						<span class="dot"></span>
						<span class="aname">{a.name}</span>
						<span class="pill">{a.status}</span>
						<span class="aline">{a.line}</span>
					</button>
				{/each}
			</div>
		</div>
	{/each}
</section>
