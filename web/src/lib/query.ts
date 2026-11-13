import { page } from '$app/state';
import { goto } from '$app/navigation';

export type QueryOptions = {
	/** Drop the page cursor (`before`/`previous`) — true for filter changes, false when paging. */
	resetPage?: boolean;
	/** Replace the history entry (filter edits) instead of pushing one (paging). */
	replaceState?: boolean;
};

/**
 * Update URL search params in place. An empty value removes the key. The `case` deep-link param is dropped on
 * every change unless the caller sets it, so a filter or page change never leaves a stale drawer target behind.
 */
export function setQuery(values: Record<string, string>, opts: QueryOptions = {}) {
	const { resetPage = true, replaceState = true } = opts;
	const url = new URL(page.url);
	if (resetPage) {
		url.searchParams.delete('before');
		url.searchParams.delete('previous');
	}
	if (!('case' in values)) url.searchParams.delete('case');
	for (const [key, value] of Object.entries(values)) {
		if (value) url.searchParams.set(key, value);
		else url.searchParams.delete(key);
	}
	return goto(url, { keepFocus: true, noScroll: true, replaceState });
}
