import { describe, expect, it } from 'vitest';
import {
	armedFaults,
	cellText,
	changedEditable,
	FAULT_TOOLS,
	faultToolLabel,
	isDemoTable,
	rowKey,
	TABLES,
	tableLabel,
} from '../../src/routes/demo/tables';
import type { DemoRecord, DemoTablePage, FaultState } from '../../src/lib/api/types';

// Inline inputs only (F7): these tests assert behaviour, not the values of src/lib/api/fixtures.
const page: DemoTablePage = {
	table: 'orders',
	primary_key: 'order_id',
	editable: ['status', 'shipment_id'],
	columns: ['order_id', 'status', 'shipment_id', 'amount_cents'],
	rows: [{ order_id: 'ORD-1', status: 'placed', shipment_id: null, amount_cents: 100 }],
	total: 1,
};

describe('demo tables', () => {
	it('knows exactly the eight tables demo-backends exposes, in plan order', () => {
		expect(TABLES.map((t) => t.id)).toEqual([
			'customers',
			'orders',
			'shipments',
			'payments',
			'communications',
			'refunds',
			'rmas',
			'warranty_claims',
		]);
		expect(isDemoTable('orders')).toBe(true);
		expect(isDemoTable('order_lines')).toBe(false);
	});

	it('lists exactly the 11 MCP tool names demo-backends accepts', () => {
		expect(FAULT_TOOLS).toHaveLength(11);
		expect(FAULT_TOOLS).toContain('delivery_track_shipment');
		expect(FAULT_TOOLS).not.toContain('delivery');
	});

	it('keys rows by the primary key the server names', () => {
		expect(rowKey(page, page.rows[0]!)).toBe('ORD-1');
	});

	it('sends only changed editable columns', () => {
		const before: DemoRecord = { order_id: 'ORD-1', status: 'placed', shipment_id: null, amount_cents: 100 };
		const after: DemoRecord = { ...before, status: 'delayed', amount_cents: 999 };
		expect(changedEditable(page, before, after)).toEqual({ status: 'delayed' });
		expect(changedEditable(page, before, { ...before })).toEqual({});
	});

	it('lists only armed faults from the {faults: {tool: {mode, remaining}}} map', () => {
		const f: FaultState = {
			faults: {
				delivery_track_shipment: { mode: 'timeout', remaining: 1 },
				order_get_order: { mode: 'error', remaining: 0 },
			},
		};
		expect(armedFaults(f)).toEqual([{ tool: 'delivery_track_shipment', mode: 'timeout', remaining: 1 }]);
		expect(armedFaults({ faults: {} })).toEqual([]);
	});
});

describe('demo table presentation', () => {
	it('renders objects as compact key: value pairs, not JSON', () => {
		expect(cellText({ street: 'Kade 1', city: 'Utrecht' })).toBe('street: Kade 1, city: Utrecht');
		expect(cellText({ a: { b: 1 } })).toBe('a: {b: 1}');
		expect(cellText(['x', 2])).toBe('x, 2');
		expect(cellText([{ sku: 'A' }])).toBe('{sku: A}');
		expect(cellText(null)).toBe('—');
		expect(cellText({})).toBe('—');
		expect(cellText(0)).toBe('0');
		expect(cellText(false)).toBe('false');
	});
	it('labels every fault tool and every table in plain language', () => {
		for (const t of FAULT_TOOLS) expect(faultToolLabel(t), t).not.toBe(t);
		expect(faultToolLabel('unknown_tool')).toBe('unknown_tool');
		expect(tableLabel('rmas')).toBe('Returns (RMA)');
		expect(tableLabel('nope')).toBe('nope');
	});
});
