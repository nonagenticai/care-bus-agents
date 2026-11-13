import { describe, expect, it } from 'vitest';
import {
	agentName,
	areaLabel,
	outcomeLabel,
	spaceLabels,
} from '../../src/routes/admin/jobs/[job]/cases/lanes';
import { auditSummary, actorTypeLabel, resourceTypeLabel } from '../../src/routes/admin/audit/labels';
import type { AuditRow, ContextSpace } from '../../src/lib/api/types';

// Inline inputs (F7).
const space = {
	key: 'retail:order-support:returns',
	area: 'order-support',
	label: 'Returns',
	request_type: 'return',
	procedure: 'return',
} as ContextSpace;

describe('case labels', () => {
	it('areas, outcomes and publishers read as words', () => {
		expect(areaLabel('order-support')).toBe('Order support');
		expect(areaLabel('faq')).toBe('FAQ');
		expect(outcomeLabel(null)).toBe('Open');
		expect(outcomeLabel('not_eligible')).toBe('Not eligible');
		expect(agentName('worker:order')).toBe('order worker');
		expect(agentName('mcp:order')).toBe('Tool executor (MCP bridge)');
		expect(agentName('somebody')).toBe('somebody');
	});

	it('context spaces label by key and request type, falling back to the raw value', () => {
		const l = spaceLabels([space]);
		expect(l.space('retail:order-support:returns')).toBe('Returns');
		expect(l.type('return')).toBe('Returns');
		expect(l.type('payment_error')).toBe('payment error');
		expect(spaceLabels([]).space('x:y')).toBe('x:y');
	});

	it('names the procedure a case space runs, so the row sub-line is not the label again', () => {
		const l = spaceLabels([space]);
		expect(l.procedure('retail:order-support:returns')).toBe('return');
		expect(l.procedure('retail:faq:general')).toBeNull();
		expect(spaceLabels([]).procedure('retail:order-support:returns')).toBeNull();
	});
});

describe('audit labels', () => {
	const row: AuditRow = {
		id: 7,
		timestamp: '2026-10-15T10:00:05.000000Z',
		actor_id: '42',
		actor_type: 'ai_agent',
		action_type: 'update',
		resource_type: 'model_route',
		resource_id: 'embeddings',
		status: 'failure',
		details: { a: 1 },
		request_id: null,
		ip_address: '10.0.0.1',
	};
	it('summarises every column except details', () => {
		expect(auditSummary(row)).toEqual([
			['When', '2026-10-15 10:00:05 UTC'],
			['Actor', 'AI agent · 42'],
			['Action', 'update'],
			['Resource', 'Model route · embeddings'],
			['Status', 'Failure'],
			['Request id', '—'],
			['IP address', '10.0.0.1'],
		]);
		expect(actorTypeLabel('robot_x')).toBe('robot x');
		expect(resourceTypeLabel('dead_letter')).toBe('Dead letter');
	});
});
