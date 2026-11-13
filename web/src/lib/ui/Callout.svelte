<script lang="ts" module>
	// Per-kind glyph so the kind is not conveyed by colour alone.
	const ICON: Record<'gap' | 'info' | 'err', string> = { gap: 'warn', info: 'info', err: 'err' };
</script>

<script lang="ts">
	import type { Snippet } from 'svelte';
	import Icon from './Icon.svelte';
	let { kind, title, children }: { kind: 'gap' | 'info' | 'err'; title: string; children: Snippet } =
		$props();
</script>

<div class="callout {kind}">
	<strong class="callout-title"><Icon name={ICON[kind]} size={16} />{title}</strong><span
		>{@render children()}</span
	>
</div>
