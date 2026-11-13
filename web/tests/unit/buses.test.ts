import { describe, expect, it } from 'vitest';
import {
	agentName,
	claimBlocked,
	entryLine,
	entryTime,
	groupChipText,
	publisherName,
	replayBlocked,
	ROLE_HINT,
	streamLabel,
} from '../../src/routes/admin/jobs/[job]/buses/labels';
import type { Entry } from '../../src/lib/api/types';

// Inline inputs (F7): every value asserted below is written in this file, not read from src/lib/api/fixtures.
describe('bus labels', () => {
	it('names every stream key shape from bus.rs and keeps unknown keys', () => {
		expect(streamLabel('bus:customer:order-support')).toBe('Order support · customer bus');
		expect(streamLabel('bus:worker:logistics:delivery')).toBe('Logistics · delivery work');
		expect(streamLabel('bus:worker:faq:results')).toBe('FAQ · work results');
		expect(streamLabel('bus:tool:business-rules')).toBe('business-rules tool calls');
		expect(streamLabel('bus:dead')).toBe('Dead letters');
		// areas are configuration: one no built-in table knows still reads as words
		expect(streamLabel('bus:customer:mars')).toBe('Mars · customer bus');
		expect(streamLabel('something:else')).toBe('something:else');
		expect(streamLabel('bus:tool')).toBe('bus:tool');
	});
	it('names agents, the recorder group and publishers; unknown ids stay', () => {
		expect(agentName('worker-order')).toBe('order worker');
		expect(agentName('carebus-api-recorder')).toBe('Case history recorder');
		expect(agentName('nobody')).toBe('nobody');
		expect(publisherName('worker:delivery')).toBe('delivery worker');
		expect(publisherName('mcp:order')).toBe('Tool executor (MCP bridge)');
	});
	it('labels the numbers on a group chip', () => {
		const g = { name: 'orchestrator', consumers: 1, last_delivered_id: '0-0' };
		expect(groupChipText({ ...g, pending: 3, lag: 0 })).toBe('Orchestrator agent · 3 pending · lag 0');
		expect(groupChipText({ ...g, pending: 0, lag: null })).toBe('Orchestrator agent · 0 pending · lag —');
	});
	it('summarises an entry as event, from → to, time and payload', () => {
		const e: Entry = {
			stream: 'bus:worker:order-support:order',
			entry_id: '1792058400000-0',
			envelope: {
				id: 'x',
				topic: 'order-support.returns.work.requested',
				from: 'orchestrator',
				correlation_id: 'CASE-1',
				published_at_ms: 1792058400000,
				tenant_id: '0195f48b-d000-7000-8000-000000000099',
				job: 'customer_care_agent',
				payload: { worker: 'order', action: 'get_order', attempt: 2 },
			},
		};
		const l = entryLine(e);
		expect(l).toMatchObject({
			event: 'work.requested',
			from: 'Orchestrator agent',
			to: 'order worker',
			detail: 'order.get_order (retry 1)',
		});
		expect(l.time).toMatch(/^\d\d:\d\d:\d\d\.\d{3}$/);
		const odd = entryLine({ ...e, envelope: { ...e.envelope, topic: 'dead.letter', from: 'worker-order' } });
		expect(odd).toMatchObject({ event: 'dead.letter', from: 'order worker', to: null, detail: '' });
	});
	it('entryTime reads the ms part of an id and is empty without one', () => {
		expect(entryTime('1792058400000-0')).toMatch(/^\d\d:/);
		expect(entryTime('0-0')).toBe('');
		expect(entryTime('nope')).toBe('');
	});
	it('gives the real reason a write button is disabled', () => {
		expect(replayBlocked(false, true)).toBe(ROLE_HINT);
		expect(replayBlocked(true, false)).toMatch(/Not a dead letter/);
		expect(replayBlocked(true, true)).toBe('');
		expect(claimBlocked(false, 4)).toBe(ROLE_HINT);
		expect(claimBlocked(true, 0)).toBe('Nothing is pending in this group');
		expect(claimBlocked(true, 1)).toBe('');
	});
});
