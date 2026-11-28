<script lang="ts">
	import type { SortState } from '$lib/sort';

	// A table header that orders the rows below it. The button keeps the whole cell clickable and keyboard
	// reachable; `aria-sort` is what a screen reader announces instead of the arrow glyph.
	let {
		key,
		label,
		sort,
		onSort,
		cls = '',
	}: {
		key: string;
		label: string;
		sort: SortState | null;
		onSort: (key: string) => void;
		cls?: string;
	} = $props();

	const dir = $derived(sort?.key === key ? sort.dir : null);
</script>

<th class={cls} aria-sort={dir === 'asc' ? 'ascending' : dir === 'desc' ? 'descending' : 'none'}>
	<button type="button" class="sort-btn" class:on={dir !== null} onclick={() => onSort(key)}>
		{label}<span class="sort-arrow" aria-hidden="true">
			{dir === 'asc' ? '▲' : dir === 'desc' ? '▼' : ''}
		</span>
	</button>
</th>
