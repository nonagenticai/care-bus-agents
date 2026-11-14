// web/tests/unit/chat-session.test.ts
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { ApiError } from '../../src/lib/api/client';
import type { ChatEvent, Conversation, MessageTurn } from '../../src/lib/api/types';
import { ChatSession, type FeedHandlers, type SessionApi } from '../../src/lib/chat/session.svelte';

const KEY = 'test.chat.conversation';
const flush = async () => {
	for (let i = 0; i < 10; i++) await Promise.resolve();
};

/** A fake carebus-api with a server-side log; `reply` decides each POST's outcome. */
function fakeServer() {
	const log: ChatEvent[] = [];
	let at = 0;
	const s = {
		log,
		posts: [] as string[],
		starts: 0,
		feeds: [] as { h: FeedHandlers; started: boolean; stopped: boolean }[],
		reply: (text: string): MessageTurn | Promise<MessageTurn> => {
			log.push({ conversation_id: 'conv-1', role: 'customer', text, at_ms: ++at });
			log.push({ conversation_id: 'conv-1', role: 'agent', text: `Reply to ${text}`, at_ms: ++at });
			return { turn: 'ask', text: `Reply to ${text}`, case_id: null };
		},
		conversation: vi.fn(async (id: string): Promise<Conversation> => ({
			conversation_id: id,
			customer_id: 'C-1',
			messages: [...log],
			case_ids: [],
		})),
		api: null as unknown as SessionApi,
	};
	s.api = {
		startConversation: async () => {
			s.starts++;
			return { conversation_id: 'conv-1', customer_id: 'C-1', customer_name: 'Anna', customer_tier: 'Gold' };
		},
		sendMessage: async (_id: string, text: string) => {
			s.posts.push(text);
			return s.reply(text);
		},
		conversation: s.conversation,
		case: vi.fn(),
		eventsUrl: (id: string) => `http://api.test/events?conversation_id=${id}`,
		scopeHeaders: () => ({}),
		demoRecord: async () => ({ name: 'Anna', tier: 'Gold' }),
	} as unknown as SessionApi;
	return s;
}

function session(
	s: ReturnType<typeof fakeServer>,
	extra: Partial<ConstructorParameters<typeof ChatSession>[0]> = {},
) {
	return new ChatSession({
		api: () => s.api,
		token: async () => 'tok',
		storageKey: KEY,
		trace: false,
		createFeed: (h) => {
			const f = { h, started: false, stopped: false };
			s.feeds.push(f);
			return {
				start: () => (f.started = true),
				stop: () => (f.stopped = true),
			};
		},
		...extra,
	});
}

beforeEach(() => sessionStorage.clear());
afterEach(() => vi.useRealTimers());

describe('ChatSession', () => {
	it('starts a conversation, opens its feed and stores its id for the tab', async () => {
		const s = fakeServer();
		const c = session(s);
		expect(await c.start()).toBe(true);
		expect(c.conversationId).toBe('conv-1');
		expect(c.customer).toEqual({ id: 'C-1', name: 'Anna', tier: 'Gold' });
		expect(s.feeds).toHaveLength(1);
		expect(s.feeds[0]!.started).toBe(true);
		expect(s.feeds[0]!.h.url).toContain('conversation_id=conv-1');
		expect(sessionStorage.getItem(KEY)).toBe('conv-1');
	});

	it('sends a message and settles once the reply is in the log (200)', async () => {
		const s = fakeServer();
		const c = session(s);
		await c.start();
		expect(c.send('Where is my airfryer?')).toBe(true);
		await flush();
		expect(s.posts).toEqual(['Where is my airfryer?']);
		expect(c.store.chat.map((m) => m.text)).toEqual([
			'Where is my airfryer?',
			'Reply to Where is my airfryer?',
		]);
		expect(c.outbox.items).toHaveLength(0);
		expect(c.inFlight).toBe(false);
		expect(c.holding).toBe(true);
	});

	it('a message sent before the conversation exists waits for the start and goes out once', async () => {
		const s = fakeServer();
		const c = session(s);
		expect(c.send('hello')).toBe(true);
		expect(c.outbox.items.map((i) => i.text)).toEqual(['hello']);
		await flush();
		await flush();
		expect(s.starts).toBe(1);
		expect(s.posts).toEqual(['hello']);
	});

	it('ignores blank input', async () => {
		const s = fakeServer();
		const c = session(s);
		expect(c.send('   ')).toBe(false);
		expect(c.outbox.items).toHaveLength(0);
	});

	it('202 queued stays in flight until the agent reply arrives over SSE', async () => {
		const s = fakeServer();
		s.reply = (text) => {
			s.log.push({ conversation_id: 'conv-1', role: 'customer', text, at_ms: 10 });
			return { turn: 'queued', text: '', case_id: null };
		};
		const c = session(s);
		await c.start();
		c.send('and it is urgent');
		await flush();
		expect(c.inFlight).toBe(true);
		expect(c.outbox.items).toHaveLength(0);
		s.feeds[0]!.h.onChat({ conversation_id: 'conv-1', role: 'agent', text: 'On it', at_ms: 11 });
		expect(c.inFlight).toBe(false);
		expect(c.store.chat.at(-1)?.text).toBe('On it');
	});

	it('a rejected send is failed; retrySend posts it again', async () => {
		const s = fakeServer();
		const ok = s.reply;
		s.reply = () => {
			throw new ApiError(400, 'invalid_body', 'text is too long');
		};
		const c = session(s);
		await c.start();
		c.send('hi');
		await flush();
		expect(c.failedSend).toBe(true);
		expect(c.unknown).toBe(false);
		expect(c.outbox.error).toBe('text is too long');
		s.reply = ok;
		c.retrySend();
		await flush();
		expect(s.posts).toEqual(['hi', 'hi']);
		expect(c.failedSend).toBe(false);
		expect(c.outbox.items).toHaveLength(0);
	});

	it('a lost response is unknown: new messages pause until it is resent or discarded', async () => {
		const s = fakeServer();
		s.reply = () => {
			throw new ApiError(0, 'network', 'Failed to fetch');
		};
		const c = session(s);
		await c.start();
		c.send('hi');
		await flush();
		expect(c.unknown).toBe(true);
		expect(c.failedSend).toBe(false);
		expect(c.send('another')).toBe(false);
		c.discard();
		expect(c.unknown).toBe(false);
		expect(c.outbox.items).toHaveLength(0);
	});

	it('keeps the outbox across a reload and resumes the stored conversation without starting a new one', async () => {
		const s = fakeServer();
		s.reply = () => {
			throw new ApiError(400, 'bad', 'rejected');
		};
		const c = session(s);
		await c.start();
		c.send('kept');
		await flush();
		c.destroy();
		const again = session(s);
		await again.start();
		expect(s.starts).toBe(1);
		expect(again.conversationId).toBe('conv-1');
		expect(again.outbox.items.map((i) => [i.text, i.status])).toEqual([['kept', 'failed']]);
	});

	it('the URL conversation wins over the stored one, and is written back', async () => {
		sessionStorage.setItem(KEY, 'conv-old');
		const s = fakeServer();
		const written: (string | null)[] = [];
		const c = session(s, { readUrl: () => 'conv-1', writeUrl: (id) => written.push(id) });
		await c.start();
		expect(s.conversation).toHaveBeenCalledWith('conv-1');
		expect(s.conversation).not.toHaveBeenCalledWith('conv-old');
		expect(written).toEqual(['conv-1']);
	});

	it('a conversation the server no longer has (404) is replaced; any other failure keeps it', async () => {
		const s = fakeServer();
		sessionStorage.setItem(KEY, 'gone');
		s.conversation.mockRejectedValueOnce(new ApiError(404, 'not_found', 'no such conversation'));
		const c = session(s);
		expect(await c.start()).toBe(true);
		expect(c.conversationId).toBe('conv-1');

		const t = fakeServer();
		sessionStorage.setItem(KEY, 'conv-9');
		t.conversation.mockRejectedValueOnce(new ApiError(503, 'unavailable', 'down'));
		const d = session(t);
		expect(await d.start()).toBe(false);
		expect(d.unreachable).toBe('conv-9');
		expect(d.error).toBe('down');
		expect(t.starts).toBe(0);
		expect(sessionStorage.getItem(KEY)).toBe('conv-9');
	});

	it('reset drops the conversation, its outbox and its feed, and starts a new one', async () => {
		const s = fakeServer();
		const c = session(s);
		await c.start();
		c.send('hi');
		await flush();
		c.reset();
		expect(s.feeds[0]!.stopped).toBe(true);
		expect(c.store.chat).toHaveLength(0);
		await flush();
		expect(s.starts).toBe(2);
		expect(c.conversationId).toBe('conv-1');
	});

	it('loads case traces onto the bus lanes only when asked to (the console)', async () => {
		const s = fakeServer();
		s.reply = (text) => {
			s.log.push({ conversation_id: 'conv-1', role: 'customer', text, at_ms: 1 });
			return { turn: 'published', text: 'checking', case_id: 'CASE-1' };
		};
		(s.api.case as ReturnType<typeof vi.fn>).mockResolvedValue({ trace: [] });
		const onActivity = vi.fn();
		const c = session(s, { trace: true, onActivity });
		await c.start();
		c.send('where is it');
		await flush();
		expect(s.api.case).toHaveBeenCalledWith('CASE-1');
		expect(onActivity).toHaveBeenCalled();

		const t = fakeServer();
		t.reply = s.reply;
		const d = session(t);
		await d.start();
		d.send('where is it');
		await flush();
		expect(t.api.case).not.toHaveBeenCalled();
	});
});
