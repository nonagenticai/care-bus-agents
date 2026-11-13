import { ApiError, type Api } from '$lib/api/client';
import { EventFeed, type FeedStatus } from '$lib/api/sse';
import type { BusAck, BusEvent, ChatEvent, MessageTurn } from '$lib/api/types';
import { ConsoleStore, Epoch } from '$lib/console/console.svelte';
import {
	CONVERSATION_KEY,
	type Batch,
	type Outbox,
	adopt,
	discardUnsent,
	dropOutbox,
	emptyOutbox,
	failed,
	hasUnknown,
	inFlight,
	loadOutbox,
	posted,
	readValue,
	reconcile,
	release,
	replied,
	requeueSending,
	resendUnknown,
	retryFailed,
	saveOutbox,
	submit,
	take,
	turnAfter,
	writeValue,
} from '$lib/console/outbox';

/** The conversation routes a session uses (all job-scoped through client.ts `J()`). */
export type SessionApi = Pick<
	Api,
	'startConversation' | 'sendMessage' | 'conversation' | 'case' | 'eventsUrl' | 'scopeHeaders' | 'demoRecord'
>;
/** What the session needs from a live feed; `EventFeed` in the app, a fake in tests. */
export interface Feed {
	start(): void;
	stop(): void;
}
export interface FeedHandlers {
	url: string;
	token: (opts?: { force?: boolean }) => Promise<string | null>;
	headers: () => Record<string, string>;
	onBus: (e: BusEvent) => void;
	onAck: (a: BusAck) => void;
	onChat: (e: ChatEvent) => void;
	onReconnect: () => Promise<void>;
	onStatus: (s: FeedStatus) => void;
	idleMs: number;
}
export interface SessionOpts {
	api: () => SessionApi;
	token: (opts?: { force?: boolean }) => Promise<string | null>;
	createFeed?: (h: FeedHandlers) => Feed;
	/** sessionStorage key of the conversation this tab resumes; each page keeps its own. */
	storageKey?: string;
	/** `?conversation=` in the URL, which wins over the stored one. */
	readUrl?: () => string | null;
	writeUrl?: (id: string | null) => void;
	/** Load case traces onto the bus lanes (the console); the customer chat needs only the chat log. */
	trace?: boolean;
	/** A new bus event arrived or a case trace was loaded. */
	onActivity?: () => void;
	/** A conversation (new or resumed) is being opened. */
	onStart?: () => void;
	busyRetryMs?: number;
}
export type Customer = { id: string; name: string | null; tier: string | null };

/**
 * One customer conversation: start or resume it, post messages through the outbox (200 reply, or 202 queued),
 * follow replies over the chat SSE, and keep unsent messages across a reload. Shared by the console and the
 * customer chat page; each owns its own instance.
 */
export class ChatSession {
	readonly store = new ConsoleStore();
	conversationId = $state<string | null>(null);
	customer = $state<Customer | null>(null);
	lastTurn = $state<Exclude<MessageTurn['turn'], 'queued'> | null>(null);
	feedStatus = $state<FeedStatus>('stopped');
	error = $state('');
	/** A conversation from the URL/tab that could not be loaded (not a 404): kept, so a retry resumes it. */
	unreachable = $state<string | null>(null);
	#outbox = $state<Outbox>(emptyOutbox());

	private feed: Feed | null = null;
	/** The start in progress; a send made before the conversation exists waits for it instead of starting another. */
	private starting: Promise<boolean> | null = null;
	/** While a conversation is being resumed, nothing may be posted before stored batches are reconciled. */
	private resuming = false;
	/** After a 409, wait for a fresh agent event (or the fallback timer) before trying again. */
	private busyHold = false;
	private busyTimer: ReturnType<typeof setTimeout> | null = null;
	/** Bumped by start(), reset() and destroy: work captured before an await is dropped when it no longer matches. */
	private readonly epoch = new Epoch();
	private readonly key: string;

	constructor(private readonly o: SessionOpts) {
		this.key = o.storageKey ?? CONVERSATION_KEY;
	}

	get outbox(): Outbox {
		return this.#outbox;
	}
	/** Every outbox change is stored for the open conversation, so a reload keeps unsent messages. */
	set outbox(v: Outbox) {
		this.#outbox = v;
		if (this.conversationId) saveOutbox(this.conversationId, v);
	}
	/** A turn is in flight: the POST is outstanding or its reply has not arrived. */
	get inFlight(): boolean {
		return inFlight(this.#outbox);
	}
	/** A send whose outcome is unknown: new messages wait until the customer settles it. */
	get unknown(): boolean {
		return hasUnknown(this.#outbox);
	}
	/** A send the server rejected (resend is safe). */
	get failedSend(): boolean {
		return (
			!this.unknown && (this.#outbox.error !== null || this.#outbox.items.some((i) => i.status === 'failed'))
		);
	}
	/** The agent asked for more and nothing new is on its way. */
	get holding(): boolean {
		return (this.lastTurn === 'ask' || this.lastTurn === 'unclear') && !this.inFlight;
	}

	/**
	 * Moves the outbox forward: drop batches the server has echoed, notice the agent's reply, and post the next
	 * batch when no turn is in flight. Called after every chat change and every POST outcome.
	 */
	private pump() {
		if (!this.conversationId || this.resuming) return;
		this.outbox = reconcile(this.outbox, this.store.chat);
		if (!this.busyHold && this.store.replied()) this.outbox = replied(this.outbox);
		const next = take(this.outbox, this.store.customers);
		this.outbox = next.o;
		if (next.batch) void this.post(next.batch);
	}

	private async post(batch: Batch) {
		const id = this.conversationId;
		if (!id) return;
		const g = this.epoch.now();
		this.store.beginSend();
		try {
			// The reply (question, or "checking this now") also arrives as a `chat` event; SSE drives the log.
			const turn = await this.o.api().sendMessage(id, batch.text);
			if (this.epoch.stale(g)) return;
			// 202 `queued`: accepted, answered after the running turn; it stays in flight ("replying…").
			this.lastTurn = turnAfter(this.lastTurn, turn.turn);
			this.outbox = posted(this.outbox, batch.id);
			this.error = '';
			// The customer turn is now in the server log; load it so the reply can be matched to it.
			try {
				await this.rebuild();
			} catch {
				/* best effort: SSE or the next reconnect rebuild delivers it */
			}
			if (this.epoch.stale(g)) return;
			if (turn.case_id && this.o.trace !== false) {
				// The server's SSE filter drops request.qualified, which is published before the case is
				// linked to the conversation, so fetch the case trace (addBus dedupes by stream + entry_id).
				try {
					const detail = await this.o.api().case(turn.case_id);
					if (this.epoch.stale(g)) return;
					for (const e of detail.trace) this.store.addBus(e);
					this.o.onActivity?.();
				} catch {
					/* best effort: SSE or the next reconnect rebuild delivers the rest */
				}
			}
		} catch (e) {
			if (this.epoch.stale(g)) return;
			this.store.cancelSend();
			const busy = e instanceof ApiError && e.status === 409;
			// No response (status 0) or a gateway/server error: the message may still have been stored.
			const ambiguous = e instanceof ApiError && (e.status === 0 || e.status >= 500);
			this.outbox = failed(this.outbox, batch.id, busy, (e as Error).message, ambiguous);
			if (busy) {
				// Another turn holds the conversation: keep the text queued, retry after its reply.
				this.busyHold = true;
				this.clearBusy();
				this.busyTimer = setTimeout(() => {
					this.busyTimer = null;
					this.busyHold = false;
					this.outbox = release(this.outbox);
					this.pump();
				}, this.o.busyRetryMs ?? 3000);
			}
		}
		this.pump();
	}

	private clearBusy() {
		if (this.busyTimer) clearTimeout(this.busyTimer);
		this.busyTimer = null;
	}

	/** Reloads the server's log (and, for the console, the case traces) and merges it into what SSE delivered. */
	async rebuild() {
		const g = this.epoch.now();
		if (!this.conversationId) return;
		const api = this.o.api();
		const conv = await api.conversation(this.conversationId);
		const cases = this.o.trace === false ? [] : await Promise.all(conv.case_ids.map((id) => api.case(id)));
		if (this.epoch.stale(g)) return;
		this.store.rebuild(conv, cases);
		// The live `chat` event that answers a turn can be lost in an SSE gap; the rebuilt log is the truth.
		this.pump();
	}

	/** Re-attempt within the current conversation (reload its log); only start a new one when none exists. */
	async retry() {
		if (!this.conversationId) {
			await (this.starting ?? this.start());
			return;
		}
		try {
			await this.rebuild();
			this.error = '';
		} catch (e) {
			this.error = (e as Error).message;
		}
	}

	/** Resumed conversations only: GET /api/conversations/{id} carries no profile, so it is looked up here. */
	private async loadCustomer(id: string, g: number) {
		this.customer = { id, name: null, tier: null };
		try {
			const r = await this.o.api().demoRecord('customers', id);
			if (this.epoch.stale(g)) return;
			const s = (v: unknown) => (typeof v === 'string' && v ? v : null);
			this.customer = { id, name: s(r.name), tier: s(r.tier) };
		} catch {
			/* the id alone is still shown */
		}
	}

	/**
	 * Resumes the conversation named in the URL or stored for this tab (so a reload keeps its outbox), or starts
	 * a new one. Resolves true when a conversation and its feed are live; false on error or when superseded.
	 */
	start(fresh = false): Promise<boolean> {
		const p = this.begin(fresh);
		this.starting = p;
		void p.finally(() => {
			if (this.starting === p) this.starting = null;
		});
		return p;
	}

	private async begin(fresh: boolean): Promise<boolean> {
		const g = this.epoch.bump();
		this.feed?.stop();
		this.feed = null;
		this.error = '';
		const api = this.o.api();
		try {
			let id = fresh ? null : (this.o.readUrl?.() ?? readValue(this.key));
			let customerId = '';
			// From POST /api/conversations (the server reads the demo customers table); null on a resume.
			let profile: { name: string | null; tier: string | null } | null = null;
			if (id) {
				try {
					customerId = (await api.conversation(id)).customer_id;
				} catch (e) {
					// Only a conversation the server says does not exist is replaced. Any other failure (network,
					// 5xx, 401) keeps the id, the URL and its stored outbox; "Reload conversation" tries again.
					if (!(e instanceof ApiError && e.status === 404)) {
						if (!this.epoch.stale(g)) this.unreachable = id;
						throw e;
					}
					id = null;
				}
				if (this.epoch.stale(g)) return false;
			}
			if (!id) {
				const r = await api.startConversation();
				if (this.epoch.stale(g)) return false;
				id = r.conversation_id;
				customerId = r.customer_id;
				profile = { name: r.customer_name, tier: r.customer_tier };
			}
			const cid = id;
			this.unreachable = null;
			writeValue(this.key, cid);
			this.o.writeUrl?.(cid);
			this.resuming = true;
			this.conversationId = cid;
			// Keep what was typed before the conversation existed: it is sent once the stored batches are settled.
			this.outbox = adopt(loadOutbox(cid), this.outbox);
			const handlers: FeedHandlers = {
				url: api.eventsUrl(cid),
				token: this.o.token,
				headers: () => api.scopeHeaders(),
				onBus: (e) => {
					if (this.store.addBus(e)) this.o.onActivity?.();
				},
				onAck: (a) => this.store.addAck(a),
				onChat: (e) => {
					this.store.addChat(e);
					if (e.role === 'agent' && this.busyHold) {
						this.busyHold = false;
						this.clearBusy();
						this.outbox = release(this.outbox);
					}
					this.pump();
				},
				onReconnect: () => this.rebuild(),
				onStatus: (s) => (this.feedStatus = s),
				idleMs: 35_000,
			};
			this.feed = (this.o.createFeed ?? ((h) => new EventFeed(h)))(handlers);
			this.feed.start();
			// A start whose lookup failed answers nulls: the id alone is shown, no second lookup.
			if (profile) this.customer = { id: customerId, ...profile };
			else void this.loadCustomer(customerId, g);
			this.o.onStart?.();
			try {
				await this.rebuild();
			} catch {
				/* the feed's reconnect rebuild retries */
			}
			if (this.epoch.stale(g)) return false;
			// A batch stored mid-POST either reached the log (reconcile drops it) or is sent again.
			this.outbox = requeueSending(reconcile(this.outbox, this.store.chat));
			this.resuming = false;
			this.pump();
			return true;
		} catch (e) {
			this.resuming = false;
			if (!this.epoch.stale(g)) this.error = (e as Error).message;
			return false;
		}
	}

	/**
	 * Never blocks: the message shows at once and goes out now, or after the agent's current reply. False when
	 * nothing was taken (blank, or an unknown send is waiting to be settled), so the composer keeps its text.
	 */
	send(msg: string): boolean {
		if (!msg.trim() || this.unknown) return false;
		this.outbox = submit(this.outbox, msg);
		// The first message may arrive before the conversation exists: it stays queued in the outbox and start()
		// adopts it. An in-progress start is awaited, never superseded by a second one.
		void (async () => {
			if (!this.conversationId && !(await (this.starting ?? this.start()))) return;
			this.pump();
		})();
		return true;
	}

	retrySend() {
		this.outbox = retryFailed(this.outbox);
		this.pump();
	}
	resendAnyway() {
		this.outbox = resendUnknown(this.outbox);
		this.pump();
	}
	discard() {
		this.outbox = discardUnsent(this.outbox);
	}

	/** Drops this conversation (and its unsent messages) and starts a new one. */
	reset() {
		this.epoch.bump();
		this.feed?.stop();
		this.feed = null;
		if (this.conversationId) dropOutbox(this.conversationId);
		writeValue(this.key, null);
		this.o.writeUrl?.(null);
		this.unreachable = null;
		this.store.reset();
		this.conversationId = null;
		this.customer = null;
		this.outbox = emptyOutbox();
		this.lastTurn = null;
		this.busyHold = false;
		this.clearBusy();
		void this.start(true);
	}

	destroy() {
		this.epoch.bump();
		this.feed?.stop();
		this.feed = null;
		this.clearBusy();
	}
}
