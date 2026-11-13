import { ApiError } from './api/client';

export type Loadable<T> =
	| { state: 'loading' }
	| { state: 'ok'; data: T }
	| { state: 'empty'; data: T }
	| { state: 'unavailable'; reason: string; status: number };

/** D1: every list is Paged {items, next}; an empty page or an empty array is "empty", never "ok". */
function isEmptyList(d: unknown): boolean {
	if (Array.isArray(d)) return d.length === 0;
	const items = (d as { items?: unknown } | null)?.items;
	return Array.isArray(items) && items.length === 0;
}

export async function load<T>(fn: () => Promise<T>, isEmpty?: (d: T) => boolean): Promise<Loadable<T>> {
	try {
		const data = await fn();
		const empty = isEmpty ? isEmpty(data) : isEmptyList(data);
		return empty ? { state: 'empty', data } : { state: 'ok', data };
	} catch (e) {
		if (e instanceof ApiError) return { state: 'unavailable', reason: e.message, status: e.status };
		return { state: 'unavailable', reason: e instanceof Error ? e.message : String(e), status: -1 };
	}
}

/**
 * Latest-wins guard for async loaders: take a ticket before the await, drop the result if a newer ticket exists.
 * One mechanism for every list refresh and detail drawer.
 */
export class Latest {
	private n = 0;
	next(): number {
		return ++this.n;
	}
	stale(ticket: number): boolean {
		return ticket !== this.n;
	}
}

export class Resource<T> {
	value = $state<Loadable<T>>({ state: 'loading' });
	refreshing = $state(false);
	updatedAt = $state<Date | null>(null);
	private latest = new Latest();
	constructor(
		private fn: () => Promise<T>,
		private isEmpty?: (d: T) => boolean,
	) {}
	/** Bound refresh, safe to pass straight through: `<Unavailable reason=… retry={res.retry} />`. */
	readonly retry = (): Promise<void> => this.refresh();
	async refresh() {
		const ticket = this.latest.next();
		this.refreshing = true;
		const next = await load(this.fn, this.isEmpty);
		if (this.latest.stale(ticket)) return;
		this.value = next;
		this.refreshing = false;
		if (next.state !== 'unavailable') this.updatedAt = new Date();
	}
}
