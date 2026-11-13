<script lang="ts" module>
	export const JSON_CAP = 20_000;
	export type Tok = { t: string; c?: 'k' | 's' | 'n' | 'b' };
	// Keys, strings, numbers and true/false/null in JSON.stringify output.
	const RE = /("(?:\\.|[^"\\])*")(\s*:)?|(-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?)|\b(true|false|null)\b/g;
	/** Splits pretty-printed JSON into coloured tokens (.k key, .s string, .n number, .b bool/null). */
	export function tokenize(src: string): Tok[] {
		const out: Tok[] = [];
		let i = 0;
		for (const m of src.matchAll(RE)) {
			if (m.index > i) out.push({ t: src.slice(i, m.index) });
			if (m[1] !== undefined) {
				out.push({ t: m[1], c: m[2] !== undefined ? 'k' : 's' });
				if (m[2]) out.push({ t: m[2] });
			} else out.push({ t: m[0], c: m[3] !== undefined ? 'n' : 'b' });
			i = m.index + m[0].length;
		}
		if (i < src.length) out.push({ t: src.slice(i) });
		return out;
	}
</script>

<script lang="ts">
	let { value, cap = JSON_CAP }: { value: unknown; cap?: number } = $props();
	let showAll = $state(false);
	let copied = $state('');

	const text = $derived(JSON.stringify(value, null, 2) ?? String(value));
	const truncated = $derived(!showAll && text.length > cap);
	const shown = $derived(truncated ? text.slice(0, cap) : text);
	const tokens = $derived(tokenize(shown));

	async function copy() {
		try {
			await navigator.clipboard.writeText(text);
			copied = 'Copied';
		} catch {
			copied = 'Copy failed';
		}
		setTimeout(() => (copied = ''), 1600);
	}
</script>

<div class="json-view">
	<div class="json-tools">
		<span role="status">{copied}</span>
		<button class="btn sm" type="button" onclick={copy}>Copy</button>
	</div>
	<pre class="code json">{#each tokens as tok}{#if tok.c}<span class={tok.c}>{tok.t}</span
				>{:else}{tok.t}{/if}{/each}{#if truncated}…{/if}</pre>
	{#if truncated}
		<div class="json-tools">
			<span>Showing {cap.toLocaleString('en')} of {text.length.toLocaleString('en')} characters</span>
			<button class="btn sm" type="button" onclick={() => (showAll = true)}>Show all</button>
		</div>
	{/if}
</div>
