import type { ChatEvent } from '$lib/api/types';

/**
 * Customer messages typed while the agent is still answering. The composer never blocks: every message shows
 * at once as a pending bubble; while a turn is in flight it waits here, and after the agent replies everything
 * waiting goes out as ONE message (joined with "\n"). The server coalesces too: a POST that lands while another
 * turn runs (another tab, a reload race) is accepted as 202 `queued` and answered with everything else queued
 * behind that turn, so `queued` counts as sent. A 409 conversation_busy (older servers) still puts the batch back
 * in the queue instead of losing it. A definite rejection is `failed` (resend is safe). A lost response or a
 * reload mid-POST is `unknown`: the server may have stored the message, so it is never resent automatically; the
 * customer reloads the log (an echo there settles it), resends deliberately, or discards it.
 */
export type PendingStatus = 'queued' | 'sending' | 'failed' | 'unknown';
export interface Pending {
	id: number;
	text: string;
	status: PendingStatus;
	/** Batch the item was posted in; null while queued or failed. */
	batch: number | null;
	/** Customer messages already in the log when the batch was posted; its echo must come after them. */
	after: number;
}
export interface Outbox {
	items: Pending[];
	/** The POST for the current batch has not answered yet. */
	posting: boolean;
	/** The POST landed; the agent's reply to it has not arrived yet. */
	awaiting: boolean;
	error: string | null;
	nextId: number;
	nextBatch: number;
}
export interface Batch {
	id: number;
	text: string;
}

export const emptyOutbox = (): Outbox => ({
	items: [],
	posting: false,
	awaiting: false,
	error: null,
	nextId: 1,
	nextBatch: 1,
});

/** A turn is in flight: the POST is outstanding or its reply has not arrived. */
export const inFlight = (o: Outbox) => o.posting || o.awaiting;

/** The text the server stores for a batch (it trims the body). */
export const joinTexts = (texts: string[]) =>
	texts
		.map((t) => t.trim())
		.join('\n')
		.trim();

/** Adds a message to the queue; earlier failed messages go back in the queue ahead of it. Blank input changes nothing. */
export function submit(o: Outbox, text: string): Outbox {
	const t = text.trim();
	if (!t) return o;
	const r = retryFailed(o);
	return {
		...r,
		items: [...r.items, { id: r.nextId, text: t, status: 'queued', batch: null, after: 0 }],
		nextId: r.nextId + 1,
	};
}

/**
 * Messages typed before the conversation existed (the first send starts it) join the conversation's stored
 * outbox instead of being replaced by it. Only queued items carry over; they are renumbered after the stored ones.
 */
export function adopt(stored: Outbox, pending: Outbox): Outbox {
	const carried = pending.items.filter((i) => i.status === 'queued');
	if (carried.length === 0) return stored;
	let id = stored.nextId;
	return {
		...stored,
		items: [...stored.items, ...carried.map((i) => ({ ...i, id: id++, batch: null, after: 0 }))],
		nextId: id,
	};
}

/** Puts failed messages back in the queue (the customer pressed Retry, or typed something new). */
export function retryFailed(o: Outbox): Outbox {
	if (!o.items.some((i) => i.status === 'failed')) return o;
	return {
		...o,
		error: null,
		items: o.items.map((i) => (i.status === 'failed' ? { ...i, status: 'queued' as const } : i)),
	};
}

/** Messages whose delivery cannot be established; the composer holds back new sends until they are settled. */
export const hasUnknown = (o: Outbox) => o.items.some((i) => i.status === 'unknown');

/** The customer chose to send an unknown message again, accepting that it may arrive twice. */
export function resendUnknown(o: Outbox): Outbox {
	if (!hasUnknown(o)) return o;
	return {
		...o,
		error: null,
		items: o.items.map((i) =>
			i.status === 'unknown' ? { ...i, status: 'queued' as const, batch: null } : i,
		),
	};
}

/** Drops failed and unknown messages (the customer discarded them). Queued and sending ones are kept. */
export function discardUnsent(o: Outbox): Outbox {
	return {
		...o,
		error: null,
		items: o.items.filter((i) => i.status !== 'failed' && i.status !== 'unknown'),
	};
}

/**
 * When no turn is in flight, takes every queued message as one batch and marks it sending. Failed messages
 * wait for retryFailed(). `customers` is the number of customer messages already in the chat log.
 */
export function take(o: Outbox, customers: number): { o: Outbox; batch: Batch | null } {
	if (inFlight(o)) return { o, batch: null };
	const ready = o.items.filter((i) => i.status === 'queued');
	if (ready.length === 0) return { o, batch: null };
	const id = o.nextBatch;
	const items = o.items.map((i) =>
		i.status === 'queued' ? { ...i, status: 'sending' as const, batch: id, after: customers } : i,
	);
	return {
		o: { ...o, items, posting: true, awaiting: false, nextBatch: id + 1 },
		batch: { id, text: joinTexts(ready.map((i) => i.text)) },
	};
}

/**
 * The turn kind the composer reasons about after a POST. `queued` (202) says nothing about the conversation's
 * state: the earlier turn's kind still holds until the queued text is answered.
 */
export function turnAfter<T extends string>(prev: T | null, turn: T | 'queued'): T | null {
	return turn === 'queued' ? prev : turn;
}

/** The POST for `batch` answered (200, or 202 queued): the turn stays in flight until the agent's reply is seen. */
export function posted(o: Outbox, batch: number): Outbox {
	if (!o.items.some((i) => i.batch === batch) && !o.posting) return o;
	return { ...o, posting: false, awaiting: true };
}

/** The agent replied to the in-flight turn (or none is in flight): the next batch may go. */
export function replied(o: Outbox): Outbox {
	if (o.posting || !o.awaiting) return o;
	return { ...o, awaiting: false };
}

/**
 * The POST failed. `busy` (409 conversation_busy) puts the batch back in the queue and keeps waiting for the
 * reply that frees the conversation. Any other error marks the batch failed so the customer can retry it; when
 * the outcome is `ambiguous` (no response, or a gateway error) it is `unknown` instead and keeps its batch, so a
 * later echo in the log still settles it.
 */
export function failed(o: Outbox, batch: number, busy: boolean, message: string, ambiguous = false): Outbox {
	const items = o.items.map((i) =>
		i.batch === batch && i.status === 'sending'
			? busy
				? { ...i, status: 'queued' as const, batch: null }
				: ambiguous
					? { ...i, status: 'unknown' as const }
					: { ...i, status: 'failed' as const, batch: null }
			: i,
	);
	return busy
		? { ...o, items, posting: false, awaiting: true }
		: { ...o, items, posting: false, awaiting: false, error: message };
}

/** Stop waiting for a reply that may never come (a busy retry fallback); queued items can then go. */
export const release = (o: Outbox): Outbox => ({ ...o, posting: false, awaiting: false });

/**
 * Drops every sent batch whose echo (the customer message the server stored) is in the chat log, so the
 * server's message replaces the pending bubbles instead of appearing next to them. Echoes are matched by text
 * and in order: each batch takes the first unmatched customer message after its `after` mark.
 */
export function reconcile(o: Outbox, chat: ChatEvent[]): Outbox {
	const customers = chat.filter((m) => m.role === 'customer').map((m) => m.text.trim());
	const batches = [...new Set(o.items.filter((i) => i.batch !== null).map((i) => i.batch!))].sort(
		(a, b) => a - b,
	);
	const done = new Set<number>();
	let cursor = 0;
	for (const b of batches) {
		const its = o.items.filter((i) => i.batch === b);
		const text = joinTexts(its.map((i) => i.text));
		const from = Math.max(cursor, its[0]!.after);
		const at = customers.indexOf(text, from);
		if (at >= 0) {
			done.add(b);
			cursor = at + 1;
		}
	}
	if (done.size === 0) return o;
	return { ...o, items: o.items.filter((i) => i.batch === null || !done.has(i.batch)) };
}

/** On reload a batch that was mid-POST is unknown: reconcile it against the log; what remains waits for the customer. */
export const requeueSending = (o: Outbox): Outbox => ({
	...o,
	items: o.items.map((i) => (i.status === 'sending' ? { ...i, status: 'unknown' as const } : i)),
	posting: false,
	awaiting: false,
});

const KEY = (conversation: string) => `carebus.console.outbox.${conversation}`;
export const CONVERSATION_KEY = 'carebus.console.conversation';

type Store = Pick<Storage, 'getItem' | 'setItem' | 'removeItem'>;
const storage = (): Store | null => {
	try {
		return typeof sessionStorage === 'undefined' ? null : sessionStorage;
	} catch {
		return null;
	}
};

export function saveOutbox(conversation: string, o: Outbox, s: Store | null = storage()) {
	try {
		if (o.items.length === 0) s?.removeItem(KEY(conversation));
		else s?.setItem(KEY(conversation), JSON.stringify({ items: o.items, nextId: o.nextId }));
	} catch {
		/* storage blocked or full: the outbox still works for this page */
	}
}

export function loadOutbox(conversation: string, s: Store | null = storage()): Outbox {
	try {
		const raw = s?.getItem(KEY(conversation));
		if (!raw) return emptyOutbox();
		const v = JSON.parse(raw) as { items?: unknown; nextId?: unknown };
		if (!Array.isArray(v.items)) return emptyOutbox();
		const items = v.items.filter(
			(i): i is Pending =>
				typeof i === 'object' &&
				i !== null &&
				typeof (i as Pending).id === 'number' &&
				typeof (i as Pending).text === 'string' &&
				['queued', 'sending', 'failed', 'unknown'].includes((i as Pending).status),
		);
		const maxBatch = Math.max(0, ...items.map((i) => i.batch ?? 0));
		const maxId = Math.max(0, ...items.map((i) => i.id));
		return {
			...emptyOutbox(),
			items,
			nextId: Math.max(Number(v.nextId) || 0, maxId + 1),
			nextBatch: maxBatch + 1,
		};
	} catch {
		return emptyOutbox();
	}
}

export function dropOutbox(conversation: string, s: Store | null = storage()) {
	try {
		s?.removeItem(KEY(conversation));
	} catch {
		/* ignore */
	}
}

export function readValue(key: string, s: Store | null = storage()): string | null {
	try {
		return s?.getItem(key) ?? null;
	} catch {
		return null;
	}
}
export function writeValue(key: string, value: string | null, s: Store | null = storage()) {
	try {
		if (value === null) s?.removeItem(key);
		else s?.setItem(key, value);
	} catch {
		/* ignore */
	}
}
