// web/tests/unit/console-summary.test.ts
import { describe, expect, it } from 'vitest';
import type { BusEvent } from '../../src/lib/api/types';
import { ConsoleStore, MAX_PER_LANE } from '../../src/lib/console/console.svelte';
import { caseOf, links, plain } from '../../src/lib/console/bus';
import {
	discardUnsent,
	emptyOutbox,
	failed,
	hasUnknown,
	reconcile,
	resendUnknown,
	retryFailed,
	submit,
	take,
} from '../../src/lib/console/outbox';

const ev = (topic: string, payload: unknown, procedure?: string, entry = '1792065600000-0'): BusEvent => ({
	stream: 'bus:worker:logistics:order',
	entry_id: entry,
	envelope: {
		id: entry,
		topic,
		from: 'orchestrator',
		correlation_id: 'CASE-1',
		published_at_ms: 1,
		tenant_id: '0195f48b-d000-7000-8000-000000000099',
		job: 'customer_care_agent',
		payload,
		procedure,
	},
	consumer_group: null,
	acked: null,
});
const T = 'logistics.delivery-status.';

describe('plain-language summaries use the real topics and fields', () => {
	it('request.qualified', () => {
		expect(
			plain(ev(`${T}request.qualified`, { request_type: 'delivery_status', slots: { order_id: 'ORD-1' } })),
		).toBe('Customer request understood as delivery status (ORD-1); handed to the orchestrator.');
	});
	it('work.requested / succeeded / failed read worker + action', () => {
		const w = { worker: 'order', action: 'get_order', attempt: 2 };
		expect(plain(ev(`${T}work.requested`, w))).toBe(
			'Orchestrator asked the order worker to get order (retry 1).',
		);
		expect(plain(ev(`${T}work.succeeded`, w))).toBe('The order worker finished: get order.');
		expect(plain(ev(`${T}work.failed`, { ...w, error: 'timeout' }))).toBe(
			'The order worker could not get order: timeout.',
		);
	});
	it('tool.call and tool.response read mcp_tool', () => {
		expect(
			plain(ev(`${T}tool.call`, { mcp_tool: 'orders_get', arguments: {}, work: { action: 'get_order' } })),
		).toBe('Calling enterprise tool orders_get.');
		expect(plain(ev(`${T}tool.response`, { mcp_tool: 'orders_get', ok: true, latency_ms: 12 }))).toBe(
			'Tool orders_get answered in 12 ms.',
		);
		expect(plain(ev(`${T}tool.response`, { mcp_tool: 'orders_get', ok: false, error: 'boom' }))).toBe(
			'Tool orders_get failed: boom.',
		);
	});
	it('request.resolved maps each outcome', () => {
		expect(plain(ev(`${T}request.resolved`, { outcome: 'completed' }))).toBe('Case completed.');
		expect(plain(ev(`${T}request.resolved`, { outcome: 'not_eligible', reason: 'window closed' }))).toBe(
			'Case closed as not eligible: window closed.',
		);
		expect(plain(ev(`${T}request.resolved`, { outcome: 'failed' }))).toBe('Case failed.');
	});
	it('topics the backend never emits get no invented sentence', () => {
		expect(plain(ev(`${T}work.completed`, { tool: 'x' }))).toBe('');
		expect(plain(ev(`${T}case.resolved`, {}))).toBe('');
	});
});

describe('links only name ids that are in the event', () => {
	it('case from payload, tool from mcp_tool, procedure from the envelope', () => {
		const e = ev(`${T}tool.call`, { mcp_tool: 'orders_get', work: { case_id: 'CASE-9' } }, 'returns@3');
		expect(caseOf(e)).toBe('CASE-9');
		expect(links(e)).toEqual([
			{ kind: 'case', label: 'CASE-9', href: '/admin/cases?case=CASE-9' },
			{ kind: 'tool', label: 'orders_get', href: '/admin/tools' },
			{ kind: 'procedure', label: 'returns@3', href: '/admin/procedures' },
		]);
	});
	it('does not fall back to the correlation id', () => {
		expect(links(ev(`${T}tool.response`, { ok: true }))).toEqual([]);
	});
});

describe('bounded history', () => {
	it('keeps the newest MAX_PER_LANE events per lane and counts the rest', () => {
		const s = new ConsoleStore();
		for (let i = 0; i < MAX_PER_LANE + 5; i++)
			s.addBus(ev(`${T}work.requested`, {}, undefined, `${1792065600000 + i}-0`));
		expect(s.worker).toHaveLength(MAX_PER_LANE);
		expect(s.trimmed).toBe(5);
		expect(s.added.worker).toBe(MAX_PER_LANE + 5);
		expect(s.worker[0]!.entry_id).toBe('1792065600005-0');
		// a rebuilt trace does not bring trimmed events back
		s.addBus(ev(`${T}work.requested`, {}, undefined, '1792065600000-0'));
		expect(s.worker).toHaveLength(MAX_PER_LANE);
	});
});

describe('ambiguous send failures', () => {
	const sent = () => {
		const o = take(submit(emptyOutbox(), 'hello'), 0);
		return o.o;
	};
	it('a lost response becomes unknown, is never auto-resent, and blocks nothing permanently', () => {
		let o = failed(sent(), 1, false, 'carebus-api is unreachable', true);
		expect(o.items[0]!.status).toBe('unknown');
		expect(hasUnknown(o)).toBe(true);
		expect(take(retryFailed(o), 0).batch).toBeNull();
		expect(take(resendUnknown(o), 0).batch?.text).toBe('hello');
		o = discardUnsent(o);
		expect(o.items).toHaveLength(0);
		expect(hasUnknown(o)).toBe(false);
	});
	it('an echo in the reloaded log settles an unknown message', () => {
		const o = failed(sent(), 1, false, 'x', true);
		const chat = [{ conversation_id: 'c1', role: 'customer' as const, text: 'hello', at_ms: 5 }];
		expect(reconcile(o, chat).items).toHaveLength(0);
	});
	it('a definite rejection stays failed and can be retried', () => {
		const o = failed(sent(), 1, false, 'rejected');
		expect(o.items[0]!.status).toBe('failed');
		expect(take(retryFailed(o), 0).batch?.text).toBe('hello');
	});
});
