<script lang="ts">
	// Read-only entity-relation graph (SvelteFlow). Deterministic columns by scope from layout.ts; no drag, connect or
	// delete — clicking a node selects the entity for the page's drawer. Colours come from the app.css tokens through
	// the --xy-* variables, so the canvas follows light and dark mode with the rest of the app.
	import { SvelteFlow, Background, Controls, BackgroundVariant, MarkerType } from '@xyflow/svelte';
	import '@xyflow/svelte/dist/style.css';
	import type { OntologyEntity, OntologyRelation } from '$lib/api/types';
	import OntologyEntityNode from './OntologyEntityNode.svelte';
	import OntologyRelationEdge from './OntologyRelationEdge.svelte';
	import { layout, type EntityNode, type RelationEdge } from './layout';

	let {
		entities,
		relations,
		selected = null,
		onselect,
	}: {
		entities: OntologyEntity[];
		relations: OntologyRelation[];
		selected?: string | null;
		onselect: (entity: OntologyEntity) => void;
	} = $props();

	const nodeTypes = { entity: OntologyEntityNode };
	const edgeTypes = { relation: OntologyRelationEdge };

	let nodes = $state.raw<EntityNode[]>([]);
	let edges = $state.raw<RelationEdge[]>([]);
	$effect(() => {
		const next = layout(entities, relations);
		nodes = next.nodes.map((n) => ({ ...n, selected: n.id === selected }));
		edges = next.edges.map((e) => ({
			...e,
			markerEnd: { type: MarkerType.ArrowClosed, width: 14, height: 14, color: 'var(--faint)' },
		}));
	});
</script>

<div class="onto-graph" dir="ltr">
	<SvelteFlow
		bind:nodes
		bind:edges
		{nodeTypes}
		{edgeTypes}
		fitView
		minZoom={0.3}
		nodesDraggable={false}
		nodesConnectable={false}
		elementsSelectable={true}
		deleteKey={null}
		proOptions={{ hideAttribution: true }}
		onnodeclick={({ node }) => onselect((node as EntityNode).data.entity)}
	>
		<Background variant={BackgroundVariant.Dots} gap={18} size={1} />
		<Controls position="bottom-left" showLock={false} />
	</SvelteFlow>
</div>

<style>
	.onto-graph {
		position: relative;
		width: 100%;
		height: 100%;
		min-height: 620px;
		background: var(--bg);
		--xy-background-color: var(--bg);
		--xy-background-pattern-dots-color: var(--line);
		--xy-edge-stroke: var(--faint);
		--xy-edge-stroke-selected: var(--accent);
		--xy-edge-label-background-color: var(--panel);
		--xy-edge-label-color: var(--muted);
		--xy-handle-background-color: var(--panel);
		--xy-handle-border-color: var(--line);
		--xy-controls-button-background-color: var(--panel);
		--xy-controls-button-background-color-hover: var(--sunk);
		--xy-controls-button-color: var(--fg);
		--xy-controls-button-color-hover: var(--fg);
		--xy-controls-button-border-color: var(--line);
		--xy-controls-box-shadow: 0 1px 3px color-mix(in srgb, var(--fg) 15%, transparent);
		--xy-attribution-background-color: transparent;
	}
	.onto-graph :global(.svelte-flow) {
		background: var(--bg);
	}
	.onto-graph :global(.svelte-flow__controls-button svg) {
		fill: currentColor;
	}
	.onto-graph :global(.svelte-flow__edge-path) {
		stroke-width: 1.25;
	}
</style>
