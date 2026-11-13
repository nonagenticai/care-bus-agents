import type { DemoRecord, DemoTable, DemoTablePage, FaultMode, FaultState } from '$lib/api/types';

/** plan 02 `SPECS`, same order. Read-only tables (refunds, rmas, warranty_claims) have `editable: []` server-side. */
export const TABLES: { id: DemoTable; label: string }[] = [
	{ id: 'customers', label: 'Customers (CRM)' },
	{ id: 'orders', label: 'Orders (OMS)' },
	{ id: 'shipments', label: 'Shipments (carrier)' },
	{ id: 'payments', label: 'Payments (PSP)' },
	{ id: 'communications', label: 'Communications' },
	{ id: 'refunds', label: 'Refunds' },
	{ id: 'rmas', label: 'Returns (RMA)' },
	{ id: 'warranty_claims', label: 'Warranty claims' },
];

const IDS = new Set<string>(TABLES.map((t) => t.id));
export const isDemoTable = (s: string): s is DemoTable => IDS.has(s);

export const PAGE_SIZE = 50;

export const rowKey = (page: Pick<DemoTablePage, 'primary_key'>, row: DemoRecord): string =>
	String(row[page.primary_key]);

/** Only the editable columns whose value changed. This is the PUT body; the server answers 422 for any other column. */
export function changedEditable(
	page: Pick<DemoTablePage, 'editable'>,
	before: DemoRecord,
	after: DemoRecord,
): DemoRecord {
	const out: DemoRecord = {};
	for (const col of page.editable) {
		if (JSON.stringify(before[col]) !== JSON.stringify(after[col])) out[col] = after[col];
	}
	return out;
}

/** Faults keyed by MCP tool name (R1). GET lists only armed faults; the filter keeps that invariant. */
export function armedFaults(f: FaultState): { tool: string; mode: FaultMode; remaining: number }[] {
	return Object.entries(f.faults)
		.filter(([, v]) => v.remaining > 0)
		.map(([tool, v]) => ({ tool, mode: v.mode, remaining: v.remaining }));
}

/** The 11 tool names demo-backends accepts (R1, feat/demo-backends faults.rs TOOL_NAMES). Anything else is a 422. */
export const FAULT_TOOLS = [
	'crm_verify_customer',
	'crm_get_start_data',
	'order_get_order',
	'order_get_payments',
	'order_create_rma',
	'order_create_warranty_claim',
	'order_issue_refund',
	'delivery_track_shipment',
	'rules_check_return_window',
	'rules_check_warranty',
	'rules_check_payment_issue',
];

/** Plain-language names for FAULT_TOOLS; the select still sends the tool name. */
export const FAULT_TOOL_LABELS: Record<string, string> = {
	crm_verify_customer: 'CRM: verify customer',
	crm_get_start_data: 'CRM: customer start data',
	order_get_order: 'Orders: get order',
	order_get_payments: 'Orders: get payments',
	order_create_rma: 'Orders: create return (RMA)',
	order_create_warranty_claim: 'Orders: create warranty claim',
	order_issue_refund: 'Orders: issue refund',
	delivery_track_shipment: 'Carrier: track shipment',
	rules_check_return_window: 'Rules: check return window',
	rules_check_warranty: 'Rules: check warranty',
	rules_check_payment_issue: 'Rules: check payment issue',
};
export const faultToolLabel = (tool: string): string => FAULT_TOOL_LABELS[tool] ?? tool;

export const tableLabel = (id: string): string => TABLES.find((t) => t.id === id)?.label ?? id;

/** One table cell: scalars as text, objects as compact `key: value` pairs, arrays comma-separated. */
export function cellText(v: unknown): string {
	if (v == null) return '—';
	if (Array.isArray(v))
		return v.length === 0
			? '—'
			: v.map((x) => (x != null && typeof x === 'object' ? `{${cellText(x)}}` : cellText(x))).join(', ');
	if (typeof v === 'object') {
		const entries = Object.entries(v as Record<string, unknown>);
		if (entries.length === 0) return '—';
		return entries
			.map(([k, x]) => `${k}: ${x != null && typeof x === 'object' ? `{${cellText(x)}}` : cellText(x)}`)
			.join(', ');
	}
	return String(v);
}
