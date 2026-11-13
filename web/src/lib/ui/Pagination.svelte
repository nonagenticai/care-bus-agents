<script lang="ts">
	import { page } from '$app/state';
	import { setQuery } from '$lib/query';
	let {
		next,
		count,
		busy = false,
		escape = false,
	}: {
		next: string | null;
		count: number;
		busy?: boolean;
		/** The page failed or came back empty: offer only the way back to newer results. */
		escape?: boolean;
	} = $props();
	const before = $derived(page.url.searchParams.get('before') ?? '');
	const previous = $derived.by(() => {
		try {
			const v: unknown = JSON.parse(page.url.searchParams.get('previous') ?? '[]');
			return Array.isArray(v) && v.every((x) => typeof x === 'string') ? (v as string[]) : [];
		} catch {
			return [];
		}
	});
	const paging = { resetPage: false, replaceState: false };
	function older() {
		if (next) void setQuery({ before: next, previous: JSON.stringify([...previous, before]) }, paging);
	}
	function newer() {
		void setQuery({ before: previous.at(-1) ?? '', previous: JSON.stringify(previous.slice(0, -1)) }, paging);
	}
</script>

<nav class="row pagination" aria-label="Results pages">
	{#if escape}
		{#if before}
			<button class="btn" disabled={busy} onclick={() => setQuery({ before: '', previous: '' }, paging)}
				>Newest results</button
			>
		{/if}
	{:else}
		<button class="btn" disabled={!before || busy} onclick={newer}>Newer results</button>
		<span role="status"
			>{count} results on this page{next ? ' · More results available' : ' · End of results'}</span
		>
		<button class="btn" disabled={!next || busy} onclick={older}>Older results</button>
		{#if before}
			<button class="btn" disabled={busy} onclick={() => setQuery({ before: '', previous: '' }, paging)}
				>Newest results</button
			>
		{/if}
	{/if}
</nav>
