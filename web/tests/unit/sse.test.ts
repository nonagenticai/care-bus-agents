// web/tests/unit/sse.test.ts
import { afterEach, describe, expect, it, vi } from 'vitest';
import type { BusEvent } from '../../src/lib/api/types';
import { EventFeed, parseSse } from '../../src/lib/api/sse';

function streamOf(chunks: string[], { close = true } = {}) {
	const enc = new TextEncoder();
	return new ReadableStream<Uint8Array>({
		start(c) {
			for (const ch of chunks) c.enqueue(enc.encode(ch));
			if (close) c.close();
		},
	});
}

describe('parseSse', () => {
	it('splits complete messages and keeps the partial tail', () => {
		const { events, rest } = parseSse(
			'event: bus\nid: 1-0\ndata: {"a":1}\n\n: ping\n\nevent: chat\nid: chat-2\ndata: {"b"',
		);
		expect(events).toEqual([{ event: 'bus', id: '1-0', data: '{"a":1}' }]);
		expect(rest).toBe('event: chat\nid: chat-2\ndata: {"b"');
	});
	it('joins multi-line data with newlines', () => {
		expect(parseSse('data: one\ndata: two\n\n').events[0]?.data).toBe('one\ntwo');
	});
});

describe('EventFeed', () => {
	const bus = {
		stream: 'bus:customer:logistics',
		entry_id: '1792065600000-0',
		envelope: {
			id: 'e1',
			topic: 'logistics.delivery-status.request.qualified',
			from: 'interaction',
			correlation_id: 'CASE-1',
			published_at_ms: 1,
			tenant_id: '0195f48b-d000-7000-8000-000000000099',
			job: 'customer_care_agent',
			payload: {},
		},
		consumer_group: 'orchestrator',
		acked: false,
	};
	const ack = {
		stream: 'bus:customer:logistics',
		entry_id: '1792065600000-0',
		consumer_group: 'orchestrator',
		correlation_id: 'CASE-1',
	};

	it('delivers bus, ack and chat events and ignores pings', async () => {
		const onBus = vi.fn(),
			onChat = vi.fn(),
			onAck = vi.fn();
		const fetchFn = vi
			.fn()
			.mockResolvedValueOnce(
				new Response(
					streamOf(
						[
							`event: bus\nid: 1792065600000-0\ndata: ${JSON.stringify(bus)}\n\n`,
							': ping\n\n',
							`event: ack\ndata: ${JSON.stringify(ack)}\n\n`,
							'event: chat\nid: chat-2\ndata: {"conversation_id":"c1","role":"agent","text":"Hi","at_ms":2}\n\n',
						],
						{ close: false },
					),
					{ status: 200 },
				),
			);
		const feed = new EventFeed({
			url: 'https://api.test/api/events?conversation_id=c1',
			token: async () => 't',
			onBus,
			onChat,
			onAck,
			onReconnect: vi.fn(),
			fetch: fetchFn,
			sleep: () => new Promise(() => {}),
		});
		feed.start();
		await vi.waitFor(() => expect(onChat).toHaveBeenCalledTimes(1));
		expect(onBus).toHaveBeenCalledWith(bus);
		expect(onAck).toHaveBeenCalledWith(ack);
		expect(fetchFn.mock.calls[0]![1].headers.Authorization).toBe('Bearer t');
		feed.stop();
	});

	it('reconnects with backoff after a drop; after a 401 it forces a token refresh; calls onReconnect', async () => {
		const sleeps: number[] = [];
		const onReconnect = vi.fn().mockResolvedValue(undefined);
		const token = vi.fn(async (o?: { force?: boolean }) => (o?.force ? 'fresh' : 'old'));
		const fetchFn = vi
			.fn()
			.mockResolvedValueOnce(new Response(streamOf([': ping\n\n']), { status: 200 })) // drops
			.mockResolvedValueOnce(new Response('', { status: 401 }))
			.mockResolvedValueOnce(new Response(streamOf([], { close: false }), { status: 200 }));
		const feed = new EventFeed({
			url: 'https://api.test/api/events',
			token,
			onBus: vi.fn(),
			onChat: vi.fn(),
			onReconnect,
			fetch: fetchFn,
			sleep: async (ms) => {
				sleeps.push(ms);
			},
		});
		feed.start();
		await vi.waitFor(() => expect(fetchFn).toHaveBeenCalledTimes(3));
		expect(sleeps).toEqual([1000, 2000]);
		expect(token).toHaveBeenLastCalledWith({ force: true });
		expect(fetchFn.mock.calls[2]![1].headers.Authorization).toBe('Bearer fresh');
		await vi.waitFor(() => expect(onReconnect).toHaveBeenCalledTimes(1)); // only after a successful re-open, not the first open
		feed.stop();
	});

	it('sends the scope headers (active tenant) with the bearer token', async () => {
		const fetchFn = vi.fn().mockResolvedValue(new Response(streamOf([], { close: false }), { status: 200 }));
		const feed = new EventFeed({
			url: 'https://api.test/api/jobs/customer_care_agent/events',
			token: async () => 't',
			headers: () => ({ 'x-active-tenant-id': '7f1c9a2e-0000-4000-8000-00000000fe12' }),
			onBus: vi.fn(),
			onChat: vi.fn(),
			onReconnect: vi.fn(),
			fetch: fetchFn,
			sleep: () => new Promise(() => {}),
		});
		feed.start();
		await vi.waitFor(() => expect(fetchFn).toHaveBeenCalledTimes(1));
		expect(fetchFn.mock.calls[0]![1].headers).toMatchObject({
			Authorization: 'Bearer t',
			'x-active-tenant-id': '7f1c9a2e-0000-4000-8000-00000000fe12',
		});
		feed.stop();
	});

	it('aborts and reconnects a connection that goes silent for idleMs (no event, no ping)', async () => {
		const signals: AbortSignal[] = [];
		const fetchFn = vi.fn(async (_url: string, init: RequestInit) => {
			signals.push(init.signal!);
			// a half-open connection: the body never yields and never closes
			return new Response(new ReadableStream<Uint8Array>({ start() {} }), { status: 200 });
		});
		const feed = new EventFeed({
			url: 'https://api.test/api/events',
			token: async () => 't',
			onBus: vi.fn(),
			onChat: vi.fn(),
			onReconnect: vi.fn(),
			fetch: fetchFn as unknown as typeof fetch,
			sleep: async () => {},
			idleMs: 50,
		});
		feed.start();
		await vi.waitFor(() => expect(fetchFn.mock.calls.length).toBeGreaterThanOrEqual(2), { timeout: 2000 });
		expect(signals[0]!.aborted).toBe(true);
		feed.stop();
	});

	it('keeps a connection that pings within idleMs', async () => {
		const enc = new TextEncoder();
		let push: (s: string) => void = () => {};
		const fetchFn = vi.fn().mockResolvedValue(
			new Response(
				new ReadableStream<Uint8Array>({
					start(c) {
						push = (s) => c.enqueue(enc.encode(s));
					},
				}),
				{ status: 200 },
			),
		);
		const feed = new EventFeed({
			url: 'u',
			token: async () => 't',
			onBus: vi.fn(),
			onChat: vi.fn(),
			onReconnect: vi.fn(),
			fetch: fetchFn,
			sleep: async () => {},
			idleMs: 80,
		});
		feed.start();
		for (let i = 0; i < 6; i++) {
			await new Promise((r) => setTimeout(r, 30));
			push(': ping\n\n');
		}
		expect(fetchFn).toHaveBeenCalledTimes(1);
		feed.stop();
	});
});

describe('EventFeed restart and failure handling', () => {
	afterEach(() => vi.useRealTimers());
	const ev = (entry_id: string) => ({
		stream: 'bus:customer:logistics',
		entry_id,
		envelope: {
			id: entry_id,
			topic: 't',
			from: 'interaction',
			correlation_id: 'CASE-1',
			published_at_ms: 1,
			tenant_id: '0195f48b-d000-7000-8000-000000000099',
			job: 'customer_care_agent',
			payload: {},
		},
	});

	it('stop() during backoff then start() runs one loop with no duplicate deliveries', async () => {
		vi.useFakeTimers();
		const onBus = vi.fn();
		const fetchFn = vi
			.fn()
			.mockResolvedValueOnce(new Response(streamOf([': ping\n\n']), { status: 200 })) // drops, then backoff
			.mockResolvedValueOnce(
				new Response(
					streamOf([`event: bus\nid: 1-0\ndata: ${JSON.stringify(ev('1-0'))}\n\n`], { close: false }),
					{ status: 200 },
				),
			);
		const feed = new EventFeed({
			url: 'u',
			token: async () => 't',
			onBus,
			onChat: vi.fn(),
			onReconnect: vi.fn(),
			fetch: fetchFn,
		});
		feed.start();
		await vi.advanceTimersByTimeAsync(0);
		expect(fetchFn).toHaveBeenCalledTimes(1);
		feed.stop();
		feed.start();
		await vi.advanceTimersByTimeAsync(5_000); // well under the 35 s idle watchdog: the stream is silent
		expect(fetchFn).toHaveBeenCalledTimes(2);
		expect(onBus).toHaveBeenCalledTimes(1);
		feed.stop();
	});

	it('a throwing handler or a malformed event skips only that event; the connection stays up', async () => {
		const warn = vi.spyOn(console, 'warn').mockImplementation(() => {});
		const onBus = vi.fn((e: BusEvent) => {
			if (e.entry_id === 'boom-0') throw new Error('handler');
		});
		const onChat = vi.fn();
		const fetchFn = vi
			.fn()
			.mockResolvedValueOnce(
				new Response(
					streamOf(
						[
							`event: bus\nid: boom-0\ndata: ${JSON.stringify(ev('boom-0'))}\n\n`,
							'event: bus\nid: bad\ndata: {not json\n\n',
							`event: bus\nid: 2-0\ndata: ${JSON.stringify(ev('2-0'))}\n\n`,
							'event: chat\nid: chat-2\ndata: {"conversation_id":"c1","role":"agent","text":"Hi","at_ms":2}\n\n',
						],
						{ close: false },
					),
					{ status: 200 },
				),
			);
		const feed = new EventFeed({
			url: 'u',
			token: async () => 't',
			onBus,
			onChat,
			onReconnect: vi.fn(),
			fetch: fetchFn,
			sleep: () => new Promise(() => {}),
		});
		feed.start();
		await vi.waitFor(() => expect(onChat).toHaveBeenCalledTimes(1));
		expect(onBus.mock.calls.map((c) => c[0].entry_id)).toEqual(['boom-0', '2-0']);
		expect(fetchFn).toHaveBeenCalledTimes(1);
		expect(warn).toHaveBeenCalledTimes(2);
		warn.mockRestore();
		feed.stop();
	});

	it('a fetch that never resolves is aborted after idleMs, then reconnects', async () => {
		vi.useFakeTimers();
		const signals: AbortSignal[] = [];
		const fetchFn = vi.fn((_u: string, init: RequestInit) => {
			signals.push(init.signal!);
			return new Promise<Response>((_, reject) => {
				init.signal!.addEventListener('abort', () => reject(new DOMException('aborted', 'AbortError')));
			});
		});
		const feed = new EventFeed({
			url: 'u',
			token: async () => 't',
			onBus: vi.fn(),
			onChat: vi.fn(),
			onReconnect: vi.fn(),
			fetch: fetchFn as unknown as typeof fetch,
			idleMs: 50,
		});
		feed.start();
		await vi.advanceTimersByTimeAsync(49);
		expect(fetchFn).toHaveBeenCalledTimes(1);
		await vi.advanceTimersByTimeAsync(1);
		expect(signals[0]!.aborted).toBe(true);
		await vi.advanceTimersByTimeAsync(1000); // default first backoff
		expect(fetchFn).toHaveBeenCalledTimes(2);
		feed.stop();
	});
});
