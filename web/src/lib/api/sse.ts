// web/src/lib/api/sse.ts
import type { BusAck, BusEvent, ChatEvent } from './types';

export type SseMessage = { event: string; id: string | null; data: string };

export function parseSse(buffer: string): { events: SseMessage[]; rest: string } {
	const norm = buffer.replace(/\r\n/g, '\n');
	const blocks = norm.split('\n\n');
	const rest = blocks.pop() ?? '';
	const events: SseMessage[] = [];
	for (const block of blocks) {
		let event = 'message',
			id: string | null = null;
		const data: string[] = [];
		for (const line of block.split('\n')) {
			if (line.startsWith(':')) continue;
			const i = line.indexOf(':');
			const field = i < 0 ? line : line.slice(0, i);
			const value = i < 0 ? '' : line.slice(i + 1).replace(/^ /, '');
			if (field === 'event') event = value;
			else if (field === 'id') id = value;
			else if (field === 'data') data.push(value);
		}
		if (data.length) events.push({ event, id, data: data.join('\n') });
	}
	return { events, rest };
}

const BACKOFF = [1000, 2000, 5000, 10000];
export type FeedStatus = 'connecting' | 'open' | 'retrying' | 'stopped';

type FeedOpts = {
	url: string;
	token: (opts?: { force?: boolean }) => Promise<string | null>;
	/** Extra request headers, read per attempt (the active tenant: `Api.scopeHeaders`). */
	headers?: () => Record<string, string>;
	onBus: (e: BusEvent) => void;
	onChat: (e: ChatEvent) => void;
	/** `event: ack`: the recipient group XACKed an entry. It may arrive before that entry's `bus` frame. */
	onAck?: (a: BusAck) => void;
	onReconnect: () => Promise<void> | void;
	onStatus?: (s: FeedStatus) => void;
	fetch?: typeof fetch;
	sleep?: (ms: number) => Promise<void>;
	/** Abort and reconnect after this many ms without a byte (D3: server pings every 15 s). */
	idleMs?: number;
};

/** One connection attempt: its controller, its idle watchdog, and how to cancel its body. */
type Attempt = { ctrl: AbortController; idle?: ReturnType<typeof setTimeout>; cancelBody?: () => void };

export class EventFeed {
	status: FeedStatus = 'stopped';
	private running = false;
	/** Bumped by stop(); a loop whose generation is stale exits at its next checkpoint. */
	private gen = 0;
	private current: Attempt | null = null;
	/** Resolves the backoff pause early, so stop() never leaves a timer or a loop behind. */
	private wake: (() => void) | null = null;
	constructor(private o: FeedOpts) {}

	start() {
		if (this.running) return;
		this.running = true;
		void this.loop(++this.gen);
	}
	stop() {
		this.running = false;
		this.gen++;
		this.wake?.();
		this.current?.ctrl.abort();
		this.current?.cancelBody?.();
		this.current = null;
		this.set('stopped');
	}
	private set(s: FeedStatus) {
		this.status = s;
		this.o.onStatus?.(s);
	}
	private get idleMs() {
		return this.o.idleMs ?? 35_000;
	}
	private arm(a: Attempt) {
		clearTimeout(a.idle);
		a.idle = setTimeout(() => {
			a.ctrl.abort();
			a.cancelBody?.();
		}, this.idleMs);
	}
	private pause(ms: number): Promise<void> {
		return new Promise<void>((resolve) => {
			let timer: ReturnType<typeof setTimeout> | undefined;
			const finish = () => {
				clearTimeout(timer);
				if (this.wake === finish) this.wake = null;
				resolve();
			};
			this.wake = finish;
			if (this.o.sleep) void this.o.sleep(ms).then(finish);
			else timer = setTimeout(finish, ms);
		});
	}
	private async loop(gen: number) {
		const f = this.o.fetch ?? fetch;
		const live = () => gen === this.gen;
		const put = (s: FeedStatus) => {
			if (live()) this.set(s);
		};
		let attempt = 0,
			everOpened = false,
			forceToken = false;
		while (live()) {
			put(attempt === 0 ? 'connecting' : 'retrying');
			const a: Attempt = { ctrl: new AbortController() };
			this.current = a;
			this.arm(a); // the watchdog covers token, fetch and headers, not only the body
			try {
				const token = await (forceToken ? this.o.token({ force: true }) : this.o.token());
				forceToken = false;
				if (!live()) break;
				const res = await f(this.o.url, {
					headers: {
						Accept: 'text/event-stream',
						...(this.o.headers?.() ?? {}),
						...(token ? { Authorization: `Bearer ${token}` } : {}),
					},
					signal: a.ctrl.signal,
				});
				if (!live()) break;
				if (res.status === 401) forceToken = true; // the server rejected a token keycloak-js thinks is valid
				if (res.ok && res.body) {
					put('open');
					if (everOpened) await this.o.onReconnect();
					if (!live()) break;
					everOpened = true;
					attempt = 0;
					await this.read(res.body, gen, a);
				} else {
					void res.body?.cancel().catch(() => {});
				}
			} catch {
				/* network error, abort, or a throwing onReconnect: fall through to backoff */
			} finally {
				clearTimeout(a.idle);
				a.cancelBody?.();
				a.ctrl.abort(); // never leave this attempt's connection open behind the next one
				if (this.current === a) this.current = null;
			}
			if (!live()) break;
			await this.pause(BACKOFF[Math.min(attempt, BACKOFF.length - 1)]!);
			if (!live()) break;
			attempt++;
		}
	}
	private async read(body: ReadableStream<Uint8Array>, gen: number, a: Attempt) {
		const reader = body.getReader();
		const dec = new TextDecoder();
		a.cancelBody = () => void reader.cancel().catch(() => {});
		let buf = '';
		for (;;) {
			const { value, done } = await reader.read();
			if (done || gen !== this.gen) return;
			this.arm(a); // any byte, including a `: ping` comment, proves the connection is alive
			buf += dec.decode(value, { stream: true });
			const { events, rest } = parseSse(buf);
			buf = rest;
			for (const m of events) this.dispatch(m);
		}
	}
	private dispatch(m: SseMessage) {
		try {
			if (m.event === 'bus') this.o.onBus(JSON.parse(m.data) as BusEvent);
			else if (m.event === 'chat') this.o.onChat(JSON.parse(m.data) as ChatEvent);
			else if (m.event === 'ack') this.o.onAck?.(JSON.parse(m.data) as BusAck);
		} catch (err) {
			// a bad event is skipped; the connection and the rest of the batch stay up
			console.warn('sse: skipped event', m.event, m.id, err);
		}
	}
}
