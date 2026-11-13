// web/tests/unit/console-activity.test.ts
import { describe, expect, it } from 'vitest';
import type { BusEvent } from '../../src/lib/api/types';
import { activity, elapsed } from '../../src/lib/console/bus';
import {
	adopt,
	emptyOutbox,
	loadOutbox,
	posted,
	saveOutbox,
	submit,
	take,
} from '../../src/lib/console/outbox';

const T = 'logistics.delivery-status.';
const ev = (
	topic: string,
	at: number,
	cid = 'CASE-1',
	payload: unknown = {},
	stream = 'bus:customer:logistics',
) =>
	({
		stream,
		entry_id: `${at}-0`,
		envelope: { id: `${at}-0`, topic, from: 'x', correlation_id: cid, published_at_ms: at, payload },
		consumer_group: null,
		acked: null,
	}) as BusEvent;

const memory = () => {
	const mem = new Map<string, string>();
	return {
		getItem: (k: string) => mem.get(k) ?? null,
		setItem: (k: string, v: string) => void mem.set(k, v),
		removeItem: (k: string) => void mem.delete(k),
	};
};

describe('a message sent before the conversation exists', () => {
	it('survives the conversation start and is sent once it exists (regression: start() replaced the outbox)', () => {
		// send(): submit while conversationId is still null.
		const typed = submit(emptyOutbox(), 'Where is my airfryer?');
		// start(): a brand-new conversation has no stored outbox.
		const o = adopt(loadOutbox('conv-new', memory()), typed);
		expect(o.items.map((i) => [i.text, i.status])).toEqual([['Where is my airfryer?', 'queued']]);
		const next = take(o, 0);
		expect(next.batch?.text).toBe('Where is my airfryer?');
		// Exactly one batch: nothing is left queued to be posted twice.
		expect(take(posted(next.o, next.batch!.id), 0).batch).toBeNull();
	});

	it('joins a resumed conversation after its stored items, renumbered', () => {
		const s = memory();
		const stored = take(submit(submit(emptyOutbox(), 'first'), 'second'), 0).o;
		saveOutbox('conv-1', stored, s);
		const o = adopt(loadOutbox('conv-1', s), submit(emptyOutbox(), 'typed early'));
		expect(o.items.map((i) => [i.text, i.status])).toEqual([
			['first', 'sending'],
			['second', 'sending'],
			['typed early', 'queued'],
		]);
		expect(new Set(o.items.map((i) => i.id)).size).toBe(3);
		expect(o.nextId).toBeGreaterThan(Math.max(...o.items.map((i) => i.id)));
	});

	it('carries nothing when nothing was typed, so a resume is unchanged', () => {
		const stored = submit(emptyOutbox(), 'x');
		expect(adopt(stored, emptyOutbox())).toBe(stored);
	});
});

describe('activity summary is built only from observed bus events', () => {
	const events = [
		ev(`${T}request.qualified`, 1_000, 'CASE-1', {
			request_type: 'delivery_status',
			slots: { order_id: 'ORD-1' },
		}),
		ev(
			`${T}work.requested`,
			2_000,
			'CASE-1',
			{ worker: 'delivery', action: 'track_shipment' },
			'bus:worker:logistics:delivery',
		),
		ev(`${T}request.qualified`, 3_000, 'CASE-2', { request_type: 'refund_status' }),
	];

	it('follows the newest case when no filter is set', () => {
		const a = activity(events)!;
		expect(a.caseId).toBe('CASE-2');
		expect(a.events).toBe(1);
		expect(a.step).toContain('refund status');
		expect(a.outcome).toBeNull();
		expect(a.endedAt).toBeNull();
	});

	it('uses the case filter and reads the latest step of that case', () => {
		const a = activity(events, 'CASE-1')!;
		expect(a.caseId).toBe('CASE-1');
		expect(a.step).toBe('Orchestrator asked the delivery worker to track shipment.');
		expect(a.startedAt).toBe(1_000);
		expect(a.stepAt).toBe(2_000);
	});

	it('reports an outcome only once request.resolved is observed', () => {
		const done = [
			...events,
			ev(`${T}request.resolved`, 9_000, 'CASE-1', { outcome: 'failed', reason: 'carrier down' }),
		];
		const a = activity(done, 'CASE-1')!;
		expect(a.outcome).toBe('Case failed: carrier down.');
		expect(a.bad).toBe(true);
		expect(a.endedAt).toBe(9_000);
	});

	it('is empty before any event, and says so for a filtered case with no events', () => {
		expect(activity([])).toBeNull();
		const a = activity(events, 'CASE-9')!;
		expect(a.events).toBe(0);
		expect(a.step).toBe('');
		expect(a.startedAt).toBeNull();
	});

	it('formats elapsed time compactly', () => {
		expect(elapsed(-5)).toBe('0s');
		expect(elapsed(42_000)).toBe('42s');
		expect(elapsed(185_000)).toBe('3m 05s');
		expect(elapsed(2 * 3_600_000 + 7 * 60_000)).toBe('2h 07m');
		expect(elapsed(28 * 3_600_000)).toBe('1d 04h');
	});
});
