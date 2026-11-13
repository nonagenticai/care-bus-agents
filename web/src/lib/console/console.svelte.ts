import { SvelteSet } from 'svelte/reactivity';
import type { BusAck, BusEvent, CaseDetail, ChatEvent, Conversation } from '$lib/api/types';

export type Lane = 'customer' | 'worker' | 'tool' | 'dead';
export const laneOf = (stream: string): Lane =>
	stream.startsWith('bus:customer:')
		? 'customer'
		: stream.startsWith('bus:worker:')
			? 'worker'
			: stream.startsWith('bus:tool:')
				? 'tool'
				: 'dead';
const DOWN = ['.request.qualified', '.work.requested', '.tool.call'];
export const direction = (topic: string): 'down' | 'up' =>
	DOWN.some((s) => topic.endsWith(s)) ? 'down' : 'up';

/** Bus events kept per lane in the browser; older ones are dropped from the display (the case trace has them). */
export const MAX_PER_LANE = 300;

export const busKey = (e: BusEvent) => `${e.stream}|${e.entry_id}`;
const chatKey = (e: ChatEvent) => `${e.conversation_id}|${e.role}|${e.at_ms}`;
const byEntry = (a: BusEvent, b: BusEvent) => {
	const [am, as] = a.entry_id.split('-').map(Number),
		[bm, bs] = b.entry_id.split('-').map(Number);
	return am! - bm! || as! - bs!;
};

export class ConsoleStore {
	customer = $state<BusEvent[]>([]);
	worker = $state<BusEvent[]>([]);
	tool = $state<BusEvent[]>([]);
	dead = $state<BusEvent[]>([]);
	chat = $state<ChatEvent[]>([]);
	caseIds = $state<string[]>([]);
	selected = $state<BusEvent | null>(null);
	/** Events dropped from the display because a lane exceeded MAX_PER_LANE. */
	trimmed = $state(0);
	/** New events accepted per lane since the last reset (grows past the cap); drives "N new events". */
	added = $state<Record<Lane, number>>({ customer: 0, worker: 0, tool: 0, dead: 0 });
	/**
	 * busKeys the recipient group has XACKed. Monotonic: an acked entry never becomes pending again, so a live
	 * `ack` frame, an `acked: true` bus frame and a rebuilt trace only ever add. An `ack` may arrive before its
	 * entry's `bus` frame; it is kept until the entry shows up.
	 */
	readonly acked = new SvelteSet<string>();
	private seen = new Set<string>();
	private seenChat = new Set<string>();
	private pendingAfter: number | null = null;

	/** Every bus entry of this conversation across lanes, oldest first (entry ids are millisecond-based). */
	get all(): BusEvent[] {
		return [...this.customer, ...this.worker, ...this.tool, ...this.dead].sort(byEntry);
	}
	/** Customer messages in the log; the next send's echo is the one after these. */
	get customers(): number {
		return this.chat.filter((m) => m.role === 'customer').length;
	}

	/** True only when the event was new. Lanes stay in entry_id order. */
	addBus(e: BusEvent): boolean {
		const k = busKey(e);
		if (e.acked === true) this.acked.add(k);
		if (this.seen.has(k)) return false;
		this.seen.add(k);
		const lane = laneOf(e.stream);
		this.added[lane]++;
		const entries = this[lane];
		if (!entries.length || byEntry(entries.at(-1)!, e) <= 0) entries.push(e);
		else {
			let lo = 0,
				hi = entries.length;
			while (lo < hi) {
				const mid = (lo + hi) >>> 1;
				if (byEntry(entries[mid]!, e) <= 0) lo = mid + 1;
				else hi = mid;
			}
			entries.splice(lo, 0, e);
		}
		if (entries.length > MAX_PER_LANE) {
			const old = entries.splice(0, entries.length - MAX_PER_LANE);
			this.trimmed += old.length;
			if (this.selected && old.some((o) => busKey(o) === busKey(this.selected!))) this.selected = null;
		}
		const cid = e.envelope.correlation_id;
		if (cid && !this.caseIds.includes(cid)) this.caseIds = [...this.caseIds, cid];
		return true;
	}
	/** The recipient group XACKed this entry (SSE `event: ack`). */
	addAck(a: BusAck) {
		this.acked.add(`${a.stream}|${a.entry_id}`);
	}
	/**
	 * `acked` once the entry's recipient group has XACKed it, `pending` until then, null when no group is the
	 * recipient (dead letters, unknown topics). Never inferred from replies: only from the server's ack state.
	 */
	ackOf(e: BusEvent): 'acked' | 'pending' | null {
		if (!e.consumer_group) return null;
		return this.acked.has(busKey(e)) ? 'acked' : 'pending';
	}
	/** Deduped on conversation_id|role|at_ms: SSE and the server log both deliver the same event. */
	addChat(e: ChatEvent) {
		const k = chatKey(e);
		if (this.seenChat.has(k)) return;
		this.seenChat.add(k);
		this.chat = [...this.chat, e];
	}
	/**
	 * Merges the server's log into what SSE already delivered; nothing appears twice. The trace's `acked` is
	 * recomputed by the server, so acks missed while disconnected are picked up here.
	 */
	rebuild(conv: Conversation, cases: CaseDetail[]) {
		this.caseIds = [...new Set([...this.caseIds, ...conv.case_ids])];
		const merged = [...conv.messages, ...this.chat];
		this.seenChat.clear();
		this.chat = merged
			.filter((m) => {
				const k = chatKey(m);
				if (this.seenChat.has(k)) return false;
				this.seenChat.add(k);
				return true;
			})
			.sort((a, b) => a.at_ms - b.at_ms);
		for (const c of cases) for (const e of c.trace) this.addBus(e);
	}
	/**
	 * Marks a send as in flight. The POST response carries no `at_ms`, so the pending turn is identified by
	 * position: it is the customer message after the ones already in the log.
	 */
	beginSend() {
		this.pendingAfter = this.chat.filter((m) => m.role === 'customer').length;
	}
	/** The send failed or was superseded: no turn is pending any more. */
	cancelSend() {
		this.pendingAfter = null;
	}
	/**
	 * True when the turn is answered, so the composer can unlock. With no send pending that is "the newest
	 * message is the agent's". With one pending, the log must hold the pending customer message AND an agent
	 * message after it; a log or SSE event that predates the send never answers it.
	 */
	replied(): boolean {
		if (this.pendingAfter === null) return this.chat.at(-1)?.role === 'agent';
		let customers = 0;
		let pendingSeen = false;
		for (const m of this.chat) {
			if (m.role === 'customer' && ++customers === this.pendingAfter + 1) pendingSeen = true;
			else if (pendingSeen && m.role === 'agent') return true;
		}
		return false;
	}
	reset() {
		this.customer = [];
		this.worker = [];
		this.tool = [];
		this.dead = [];
		this.chat = [];
		this.caseIds = [];
		this.selected = null;
		this.trimmed = 0;
		this.added = { customer: 0, worker: 0, tool: 0, dead: 0 };
		this.acked.clear();
		this.seen.clear();
		this.seenChat.clear();
		this.pendingAfter = null;
	}
}

/** Generation counter: capture `now()` before an await, then `stale(g)` after it says the work was superseded. */
export class Epoch {
	private n = 0;
	now(): number {
		return this.n;
	}
	bump(): number {
		return ++this.n;
	}
	stale(g: number): boolean {
		return g !== this.n;
	}
}
