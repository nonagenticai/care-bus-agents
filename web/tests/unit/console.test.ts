// web/tests/unit/console.test.ts
import { describe, expect, it } from 'vitest';
import { ConsoleStore, Epoch, direction, laneOf } from '../../src/lib/console/console.svelte';
import type { BusEvent, CaseRow, ChatEvent } from '../../src/lib/api/types';
import {
	agentCards,
	consumerOf,
	contextOf,
	eventOf,
	isBad,
	summary,
	tokens,
} from '../../src/lib/console/bus';
import {
	emptyOutbox,
	failed,
	inFlight,
	loadOutbox,
	posted,
	reconcile,
	release,
	replied,
	requeueSending,
	retryFailed,
	saveOutbox,
	submit,
	take,
	turnAfter,
} from '../../src/lib/console/outbox';

const ev = (
	stream: string,
	entry: string,
	topic: string,
	cid = 'CASE-1',
	consumer_group: string | null = null,
	acked: boolean | null = consumer_group === null ? null : false,
): BusEvent => ({
	stream,
	entry_id: entry,
	envelope: {
		id: entry,
		topic,
		from: 'x',
		correlation_id: cid,
		published_at_ms: Number(entry.split('-')[0]),
		tenant_id: '0195f48b-d000-7000-8000-000000000099',
		job: 'customer_care_agent',
		payload: {},
	},
	consumer_group,
	acked,
});
const msg = (role: ChatEvent['role'], text: string, at_ms: number): ChatEvent => ({
	conversation_id: 'c1',
	role,
	text,
	at_ms,
});
const row: CaseRow = {
	case_id: 'CASE-1',
	conversation_id: 'c1',
	customer_id: 'C-10482',
	request_type: 'delivery_status',
	area: 'logistics',
	context_space: 'retail:logistics:delivery-status',
	outcome: null,
	reason: null,
	work_items: 2,
	retries: 0,
	started_at: '2026-10-15T14:52:00Z',
	resolved_at: null,
	duration_ms: null,
	procedure: null,
};

describe('ConsoleStore.replied', () => {
	it('is true only when the newest chat message is from the agent', () => {
		const s = new ConsoleStore();
		expect(s.replied()).toBe(false);
		s.addChat(msg('customer', 'hi', 1));
		expect(s.replied()).toBe(false);
		s.addChat(msg('agent', 'hello', 2));
		expect(s.replied()).toBe(true);
		s.addChat(msg('customer', 'more', 3));
		expect(s.replied()).toBe(false);
	});
});

describe('ConsoleStore.replied with a pending send', () => {
	const exchange = () => {
		const s = new ConsoleStore();
		s.addChat(msg('customer', 'm1', 1));
		s.addChat(msg('agent', 'r1', 2));
		return s;
	};
	const conv = (messages: ChatEvent[]) => ({
		conversation_id: 'c1',
		customer_id: 'C-1',
		messages,
		case_ids: [],
	});
	it('stays unreplied when a rebuild delivers a log that predates the pending send', () => {
		const s = exchange();
		s.beginSend();
		s.rebuild(conv([msg('customer', 'm1', 1), msg('agent', 'r1', 2)]), []);
		expect(s.replied()).toBe(false);
	});
	it('stays unreplied when only the pending customer message has arrived', () => {
		const s = exchange();
		s.beginSend();
		s.rebuild(conv([msg('customer', 'm1', 1), msg('agent', 'r1', 2), msg('customer', 'm2', 3)]), []);
		expect(s.replied()).toBe(false);
	});
	it('is replied once an agent message follows the pending customer message', () => {
		const s = exchange();
		s.beginSend();
		s.rebuild(
			conv([
				msg('customer', 'm1', 1),
				msg('agent', 'r1', 2),
				msg('customer', 'm2', 3),
				msg('agent', 'r2', 4),
			]),
			[],
		);
		expect(s.replied()).toBe(true);
	});
	it('ignores a stale agent event delivered live while the send is pending', () => {
		const s = exchange();
		s.beginSend();
		s.addChat(msg('agent', 'r1 again', 2.5));
		expect(s.replied()).toBe(false);
	});
	it('cancelSend returns to the newest-message rule', () => {
		const s = exchange();
		s.beginSend();
		s.cancelSend();
		expect(s.replied()).toBe(true);
	});
	it('reset drops the pending send', () => {
		const s = exchange();
		s.beginSend();
		s.reset();
		s.addChat(msg('customer', 'a', 1));
		s.addChat(msg('agent', 'b', 2));
		expect(s.replied()).toBe(true);
	});
});

describe('ConsoleStore', () => {
	it('routes streams to lanes and topics to directions', () => {
		expect(laneOf('bus:customer:logistics')).toBe('customer');
		expect(laneOf('bus:worker:payments:order')).toBe('worker');
		expect(laneOf('bus:worker:payments:results')).toBe('worker');
		expect(laneOf('bus:tool:delivery')).toBe('tool');
		expect(laneOf('bus:dead')).toBe('dead');
		expect(direction('logistics.delivery-status.request.qualified')).toBe('down');
		expect(direction('logistics.delivery-status.work.succeeded')).toBe('up');
		expect(direction('order-support.returns.tool.response')).toBe('up');
	});

	it('drops a duplicate (same stream + entry_id) delivered twice', () => {
		const s = new ConsoleStore();
		expect(s.addBus(ev('bus:tool:order', '1792065600010-0', 'logistics.delivery-status.tool.call'))).toBe(
			true,
		);
		expect(s.addBus(ev('bus:tool:order', '1792065600010-0', 'logistics.delivery-status.tool.call'))).toBe(
			false,
		);
		expect(s.tool).toHaveLength(1);
	});

	it('orders by the numeric parts of entry_id, not as strings', () => {
		const s = new ConsoleStore();
		s.addBus(ev('bus:tool:order', '1792065600010-10', 't'));
		s.addBus(ev('bus:tool:order', '1792065600010-9', 't'));
		s.addBus(ev('bus:tool:order', '999-0', 't'));
		expect(s.tool.map((e) => e.entry_id)).toEqual(['999-0', '1792065600010-9', '1792065600010-10']);
	});

	it('rebuild after reconnect keeps each envelope once, in entry_id order, and adds the ones missed', () => {
		const s = new ConsoleStore();
		s.addBus(ev('bus:customer:logistics', '1792065600000-0', 'logistics.delivery-status.request.qualified'));
		s.addBus(ev('bus:tool:order', '1792065600010-0', 'logistics.delivery-status.tool.call'));
		// connection dropped; these two were missed
		const trace = [
			ev('bus:customer:logistics', '1792065600000-0', 'logistics.delivery-status.request.qualified'),
			ev('bus:worker:logistics:order', '1792065600005-0', 'logistics.delivery-status.work.requested'),
			ev('bus:tool:order', '1792065600010-0', 'logistics.delivery-status.tool.call'),
			ev('bus:tool:order', '1792065600020-0', 'logistics.delivery-status.tool.response'),
		];
		s.rebuild(
			{
				conversation_id: 'c1',
				customer_id: 'C-10482',
				messages: [msg('customer', 'Where is my airfryer?', 1)],
				case_ids: ['CASE-1'],
			},
			[{ case: row, hash: {}, trace }],
		);
		expect(s.customer.map((e) => e.entry_id)).toEqual(['1792065600000-0']);
		expect(s.worker.map((e) => e.entry_id)).toEqual(['1792065600005-0']);
		expect(s.tool.map((e) => e.entry_id)).toEqual(['1792065600010-0', '1792065600020-0']);
		expect(s.chat).toHaveLength(1);
		expect(s.caseIds).toEqual(['CASE-1']);
	});

	it('ack state: pending until the recipient XACKs, never inferred from a reply', () => {
		const s = new ConsoleStore();
		const call = ev('bus:tool:order', '10-0', 'a.b.tool.call', 'CASE-1', 'tool-executor');
		const resp = ev('bus:tool:order', '11-0', 'a.b.tool.response', 'CASE-1', 'worker-order');
		s.addBus(call);
		s.addBus(resp); // the reply is on the bus, but nobody has acked the call
		expect(s.ackOf(call)).toBe('pending');
		s.addAck({
			stream: 'bus:tool:order',
			entry_id: '10-0',
			consumer_group: 'tool-executor',
			correlation_id: 'CASE-1',
		});
		expect(s.ackOf(call)).toBe('acked');
		expect(s.ackOf(resp)).toBe('pending');
		// no recipient group (dead letters): no pill at all
		expect(s.ackOf(ev('bus:dead', '12-0', 'dead.letter'))).toBeNull();
	});

	it('an ack that arrives before its bus frame is kept, and a later acked:false frame never un-acks', () => {
		const s = new ConsoleStore();
		s.addAck({
			stream: 'bus:tool:order',
			entry_id: '10-0',
			consumer_group: 'tool-executor',
			correlation_id: 'CASE-1',
		});
		const call = ev('bus:tool:order', '10-0', 'a.b.tool.call', 'CASE-1', 'tool-executor', false);
		s.addBus(call);
		expect(s.ackOf(call)).toBe('acked');
		s.addBus(call);
		expect(s.ackOf(call)).toBe('acked');
		s.reset();
		expect(s.ackOf(call)).toBe('pending');
	});

	it('a reconnect rebuild flips an entry already shown as pending when the trace says it was acked', () => {
		const s = new ConsoleStore();
		const live = ev('bus:tool:order', '10-0', 'a.b.tool.call', 'CASE-1', 'tool-executor', false);
		s.addBus(live);
		expect(s.ackOf(live)).toBe('pending');
		// the ack frame was lost while disconnected; the server recomputed it into the trace
		const traced = { ...live, acked: true };
		s.rebuild({ conversation_id: 'c1', customer_id: 'C-10482', messages: [], case_ids: ['CASE-1'] }, [
			{ case: row, hash: {}, trace: [traced] },
		]);
		expect(s.tool).toHaveLength(1);
		expect(s.ackOf(live)).toBe('acked');
	});

	it('shows a chat event delivered twice (SSE, then rebuild) once', () => {
		const s = new ConsoleStore();
		const m = msg('agent', 'Checking this now.', 5);
		s.addChat(m);
		s.addChat({ ...m });
		expect(s.chat).toHaveLength(1);
		s.rebuild(
			{
				conversation_id: 'c1',
				customer_id: 'C-10482',
				messages: [msg('customer', 'Hi', 1), { ...m }],
				case_ids: [],
			},
			[],
		);
		expect(s.chat.map((c) => c.text)).toEqual(['Hi', 'Checking this now.']);
		s.addChat({ ...m });
		expect(s.chat).toHaveLength(2);
	});

	it('keeps same-text messages that differ in role or time', () => {
		const s = new ConsoleStore();
		s.addChat(msg('customer', 'ok', 1));
		s.addChat(msg('agent', 'ok', 1));
		s.addChat(msg('customer', 'ok', 2));
		expect(s.chat).toHaveLength(3);
	});

	it('reset clears lanes, chat and dedupe memory', () => {
		const s = new ConsoleStore();
		s.addBus(ev('bus:dead', '5-0', 'x'));
		s.addChat(msg('customer', 'a', 1));
		s.reset();
		expect(s.dead).toHaveLength(0);
		expect(s.chat).toHaveLength(0);
		expect(s.addBus(ev('bus:dead', '5-0', 'x'))).toBe(true);
		s.addChat(msg('customer', 'a', 1));
		expect(s.chat).toHaveLength(1);
	});
});

describe('Epoch', () => {
	it('marks work captured before a bump as stale, and later work as current', () => {
		const e = new Epoch();
		const g = e.now();
		expect(e.stale(g)).toBe(false);
		e.bump();
		expect(e.stale(g)).toBe(true);
		expect(e.stale(e.now())).toBe(false);
	});
});

describe('outbox', () => {
	const sendNow = (o: ReturnType<typeof emptyOutbox>, text: string, customers = 0) =>
		take(submit(o, text), customers);

	it('sends immediately when no turn is in flight', () => {
		const { o, batch } = sendNow(emptyOutbox(), '  Where is my airfryer?  ');
		expect(batch?.text).toBe('Where is my airfryer?');
		expect(o.items.map((i) => i.status)).toEqual(['sending']);
		expect(inFlight(o)).toBe(true);
	});

	it('ignores blank input', () => {
		const o = submit(emptyOutbox(), '   ');
		expect(o.items).toHaveLength(0);
		expect(take(o, 0).batch).toBeNull();
	});

	it('buffers while a turn is in flight, then flushes all buffered as one joined message after the reply', () => {
		let { o } = sendNow(emptyOutbox(), 'first');
		o = posted(o, 1);
		o = submit(o, 'second');
		o = submit(o, 'third');
		expect(take(o, 1).batch).toBeNull();
		expect(o.items.filter((i) => i.status === 'queued').map((i) => i.text)).toEqual(['second', 'third']);
		o = replied(o);
		const next = take(o, 1);
		expect(next.batch).toEqual({ id: 2, text: 'second\nthird' });
		expect(inFlight(next.o)).toBe(true);
	});

	it('does not treat a reply as answering a POST that is still outstanding', () => {
		const { o } = sendNow(emptyOutbox(), 'first');
		expect(inFlight(replied(o))).toBe(true);
	});

	it('treats 202 queued as sent: the batch stays in flight until the agent replies', () => {
		let { o } = sendNow(emptyOutbox(), 'hello');
		o = posted(o, 1);
		expect(o.items.map((i) => [i.text, i.status])).toEqual([['hello', 'sending']]);
		expect(inFlight(o)).toBe(true);
		o = submit(o, 'more');
		expect(take(o, 1).batch).toBeNull();
		o = replied(o);
		expect(take(o, 1).batch?.text).toBe('more');
	});

	it('a queued turn keeps the previous turn kind; any other turn replaces it', () => {
		expect(turnAfter<'ask' | 'published'>('ask', 'queued')).toBe('ask');
		expect(turnAfter<'ask' | 'published'>(null, 'queued')).toBeNull();
		expect(turnAfter<'ask' | 'published'>('ask', 'published')).toBe('published');
	});

	it('keeps the text queued on 409 conversation_busy and sends it after the next reply', () => {
		let { o } = sendNow(emptyOutbox(), 'hello');
		o = failed(o, 1, true, 'The previous message is still being handled.');
		expect(o.items.map((i) => [i.text, i.status])).toEqual([['hello', 'queued']]);
		expect(o.error).toBeNull();
		expect(take(o, 0).batch).toBeNull();
		o = submit(o, 'again');
		o = replied(o);
		expect(take(o, 0).batch?.text).toBe('hello\nagain');
	});

	it('release lets a queued batch go when no reply comes after a 409', () => {
		let { o } = sendNow(emptyOutbox(), 'hello');
		o = failed(o, 1, true, 'busy');
		expect(take(release(o), 0).batch?.text).toBe('hello');
	});

	it('marks other errors failed with a message, and resends them only on retry or a new message', () => {
		let { o } = sendNow(emptyOutbox(), 'hello');
		o = failed(o, 1, false, 'carebus-api is unreachable');
		expect(o.error).toBe('carebus-api is unreachable');
		expect(o.items[0]!.status).toBe('failed');
		expect(take(o, 0).batch).toBeNull();
		expect(take(retryFailed(o), 0).batch?.text).toBe('hello');
		const r = take(submit(o, 'more'), 0);
		expect(r.batch?.text).toBe('hello\nmore');
		expect(r.o.error).toBeNull();
	});

	it('replaces the pending bubbles with the server echo, matched by text and order', () => {
		let { o } = sendNow(emptyOutbox(), 'ok', 1);
		o = posted(o, 1);
		o = submit(o, 'later');
		const chatBefore = [msg('customer', 'ok', 1), msg('agent', 'r', 2)];
		// An older customer message with the same text is not the echo: it is before the batch's mark.
		expect(reconcile(o, chatBefore).items.map((i) => i.text)).toEqual(['ok', 'later']);
		const r = reconcile(o, [...chatBefore, msg('customer', 'ok', 3)]);
		expect(r.items.map((i) => [i.text, i.status])).toEqual([['later', 'queued']]);
	});

	it('matches a joined batch to its single joined echo', () => {
		let { o } = sendNow(emptyOutbox(), 'a');
		o = posted(o, 1);
		o = submit(o, 'b');
		o = submit(o, 'c');
		o = replied(o);
		o = take(o, 1).o;
		const chat = [msg('customer', 'a', 1), msg('agent', 'r', 2), msg('customer', 'b\nc', 3)];
		expect(reconcile(o, chat).items).toHaveLength(0);
	});

	it('round-trips through storage and requeues a batch that was mid-POST', () => {
		const mem = new Map<string, string>();
		const s = {
			getItem: (k: string) => mem.get(k) ?? null,
			setItem: (k: string, v: string) => void mem.set(k, v),
			removeItem: (k: string) => void mem.delete(k),
		};
		let { o } = sendNow(emptyOutbox(), 'one');
		o = submit(o, 'two');
		saveOutbox('c1', o, s);
		const back = loadOutbox('c1', s);
		expect(back.items.map((i) => [i.text, i.status])).toEqual([
			['one', 'sending'],
			['two', 'queued'],
		]);
		expect(inFlight(back)).toBe(false);
		// A batch that was mid-POST may have reached the server: it is never resent on its own.
		const again = take(requeueSending(back), 0);
		expect(again.batch?.text).toBe('two');
		expect(requeueSending(back).items[0]!.status).toBe('unknown');
		expect(loadOutbox('other', s).items).toHaveLength(0);
		saveOutbox('c1', emptyOutbox(), s);
		expect(mem.size).toBe(0);
	});

	it('survives storage that throws', () => {
		const bad = {
			getItem: () => {
				throw new Error('blocked');
			},
			setItem: () => {
				throw new Error('blocked');
			},
			removeItem: () => {
				throw new Error('blocked');
			},
		};
		expect(() => saveOutbox('c1', submit(emptyOutbox(), 'x'), bad)).not.toThrow();
		expect(loadOutbox('c1', bad).items).toHaveLength(0);
	});
});

describe('bus derivations', () => {
	const tc = (entry: string, call: string, tool = 'order') => ({
		...ev(`bus:tool:${tool}`, entry, 'logistics.delivery-status.tool.call', 'CASE-1', 'tool-executor'),
		envelope: {
			...ev(`bus:tool:${tool}`, entry, 'x').envelope,
			topic: 'logistics.delivery-status.tool.call',
			from: `worker:${tool}`,
			payload: { call_id: call, mcp_tool: 'order_get', arguments: { order_id: 'ORD-1' } },
		},
	});
	const tr = (entry: string, call: string, ok: boolean, tool = 'order') => ({
		...ev(`bus:tool:${tool}`, entry, 'x', 'CASE-1', `worker-${tool}`),
		envelope: {
			...ev(`bus:tool:${tool}`, entry, 'x').envelope,
			topic: 'logistics.delivery-status.tool.response',
			from: `mcp:${tool}`,
			payload: { call_id: call, ok, latency_ms: 42, error: ok ? null : 'timeout' },
		},
	});

	it('splits topics into context space and event', () => {
		expect(eventOf('logistics.delivery-status.work.failed')).toBe('work.failed');
		expect(contextOf('logistics.delivery-status.work.failed')).toBe('logistics.delivery-status.');
		expect(eventOf('something.else')).toBeNull();
	});

	it('names the consumer group from the carebus topology for /api/streams entries (no consumer_group)', () => {
		expect(consumerOf(tc('1-0', 'w1'))).toBe('tool-executor');
		expect(consumerOf(tr('2-0', 'w1', true))).toBe('worker-order');
		expect(consumerOf(ev('bus:worker:logistics:delivery', '3-0', 'a.b.work.requested'))).toBe(
			'worker-delivery',
		);
		expect(consumerOf(ev('bus:worker:logistics:results', '4-0', 'a.b.work.succeeded'))).toBe('orchestrator');
		expect(consumerOf(ev('bus:customer:faq', '5-0', 'a.b.request.resolved'))).toBe('interaction');
	});

	it('summarises payloads and flags failures', () => {
		expect(summary(tc('1-0', 'w1'))).toBe('order_get(order_id=ORD-1)');
		expect(summary(tr('2-0', 'w1', true))).toBe('ok · 42 ms');
		expect(summary(tr('2-0', 'w1', false))).toBe('error · timeout');
		expect(isBad(tr('2-0', 'w1', false))).toBe(true);
		expect(isBad(tr('2-0', 'w1', true))).toBe(false);
	});

	it("agent cards use the server's consumer_group, not a client-side topology", () => {
		// A tool.call the server says is for some other group: the executor card must not claim it.
		const odd = { ...tc('1-0', 'w1'), consumer_group: 'elsewhere' };
		const cards = agentCards([odd], null, false);
		expect(cards.find((c) => c.id === 'tool-executor')!.items).toHaveLength(0);
	});

	it('derives agent status from outstanding items and /api/agents', () => {
		const cards = agentCards([tc('1-0', 'w1')], null, false);
		const by = (id: string) => cards.find((c) => c.id === id)!;
		expect(by('tool-executor').status).toBe('working');
		expect(by('worker-order').status).toBe('waiting');
		expect(by('orchestrator').status).toBe('unknown');
		expect(by('interaction').status).toBe('unknown');
		expect(agentCards([], null, true).find((c) => c.id === 'interaction')!.status).toBe('holding');
		const done = agentCards([tc('1-0', 'w1'), tr('2-0', 'w1', false)], null, false);
		const ex = done.find((c) => c.id === 'tool-executor')!;
		expect(ex.status).toBe('unknown');
		expect(ex.items.map((w) => w.status)).toEqual(['fail']);
		expect(ex.line).toBe('→ tool.response · error · timeout');
	});

	it('tokenises JSON for colouring without losing characters', () => {
		const v = { a: 'x', n: 1.5, b: true, z: null };
		const t = tokens(v);
		expect(t.map((x) => x.t).join('')).toBe(JSON.stringify(v, null, 2));
		expect(t.filter((x) => x.c === 'k').map((x) => x.t)).toEqual(['"a"', '"n"', '"b"', '"z"']);
		expect(t.find((x) => x.c === 'n')?.t).toBe('1.5');
	});
});
