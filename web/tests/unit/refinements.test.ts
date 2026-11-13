import { describe, expect, it } from 'vitest';
import {
	SPACE_EDITABLE,
	SPACE_ROUTING,
	actionsFor,
	diffFields,
	fmtValue,
	parseList,
	sideBySide,
	spaceDraft,
	spaceFormErrors,
	spacePatch,
} from '../../src/lib/refinements';
import type { ReleaseConflict } from '../../src/lib/api/types';

const conn = (id: string, base_url: string | null, extra: Record<string, unknown> = {}) => ({
	id,
	name: id,
	base_url,
	timeout_ms: 3000,
	origin: 'base',
	overridden: false,
	hidden: false,
	health: 'unknown',
	...extra,
});

const conflict = (key: Record<string, string>, over: Partial<ReleaseConflict> = {}): ReleaseConflict => ({
	id: 1,
	kind: 'base_changed',
	object: 'connector',
	key,
	tenant_row: 'override',
	base_hash_before: 'a',
	base_hash_after: 'b',
	release_id: 'r2',
	detected_at: '2026-10-17T10:00:00Z',
	resolved_at: null,
	resolution: null,
	resolved_by: null,
	...over,
});

describe('sideBySide', () => {
	const base = [conn('crm', null), conn('oms', 'http://oms'), conn('psp', 'http://psp')];
	const tenant = [
		conn('crm', 'http://crm.local', { origin: 'tenant', overridden: true, health: 'ok' }),
		conn('psp', 'http://psp', { origin: 'tenant', overridden: true, hidden: true }),
		conn('wms', 'http://wms', { origin: 'tenant' }),
	];
	const rows = sideBySide('connector', base, tenant, [conflict({ id: 'crm' })]);

	it('classifies every key once, sorted', () => {
		expect(rows.map((r) => [r.keyText, r.state])).toEqual([
			['crm', 'override'],
			['oms', 'base'],
			['psp', 'tombstone'],
			['wms', 'extension'],
		]);
		expect(rows[0]!.key).toEqual({ id: 'crm' });
	});
	it('diffs only changed configuration fields; an unset base endpoint reads "not set"', () => {
		expect(rows[0]!.diff).toEqual([{ field: 'base_url', base: 'not set', tenant: 'http://crm.local' }]);
		expect(rows[1]!.diff).toEqual([]);
	});
	it('attaches an open conflict to its row only', () => {
		expect(rows[0]!.conflict?.kind).toBe('base_changed');
		expect(rows.slice(1).every((r) => r.conflict === null)).toBe(true);
		const resolved = sideBySide('connector', base, tenant, [
			conflict({ id: 'crm' }, { resolved_at: '2026-10-17T11:00:00Z' }),
		]);
		expect(resolved[0]!.conflict).toBeNull();
	});
	it('keys relations by subject, predicate and object', () => {
		const r: Record<string, unknown> = {
			subject: 'Customer',
			predicate: 'places',
			object: 'Order',
			cardinality: '1:n',
		};
		const [row] = sideBySide('ontology_relation', [r], [{ ...r, cardinality: 'n:m', origin: 'tenant' }]);
		expect(row!.key).toEqual({ subject: 'Customer', predicate: 'places', object: 'Order' });
		expect(row!.diff).toEqual([{ field: 'cardinality', base: '1:n', tenant: 'n:m' }]);
	});
});

describe('fmtValue and diffFields', () => {
	it('prints values for people', () => {
		expect(fmtValue(null)).toBe('not set');
		expect(fmtValue(undefined)).toBe('not set');
		expect(fmtValue([])).toBe('none');
		expect(fmtValue(['a', 'b'])).toBe('a, b');
		expect(fmtValue(42)).toBe('42');
		expect(fmtValue({ a: 1 })).toBe('{"a":1}');
	});
	it('ignores provenance and observations', () => {
		expect(
			diffFields(
				{ origin: 'base', health: 'ok', label: 'A' },
				{ origin: 'tenant', health: 'down', label: 'A' },
			),
		).toEqual([]);
	});
});

describe('actionsFor', () => {
	it('offers what the state allows, nothing when read-only', () => {
		expect(actionsFor({ state: 'override', conflict: null }, true)).toEqual(['revert']);
		expect(actionsFor({ state: 'override', conflict: conflict({ id: 'x' }) }, true)).toEqual([
			'revert',
			'rebase',
		]);
		expect(actionsFor({ state: 'tombstone', conflict: null }, true)).toEqual(['unhide']);
		expect(actionsFor({ state: 'base', conflict: null }, true)).toEqual(['hide']);
		expect(actionsFor({ state: 'extension', conflict: null }, true)).toEqual([]);
		expect(actionsFor({ state: 'override', conflict: null }, false)).toEqual([]);
	});
});

describe('release conflicts', async () => {
	const { conflictActions, conflictKeyText, conflictText } = await import('../../src/lib/conflicts');
	it('explains each kind', () => {
		for (const kind of [
			'base_changed',
			'base_removed',
			'base_added',
			'parent_missing',
			'base_shadowed',
		] as const)
			expect(conflictText({ kind })).toMatch(/\.$/);
	});
	it('offers keep-mine only where a base row exists, nothing once resolved or read-only', () => {
		expect(conflictActions({ kind: 'base_changed', resolved_at: null }, true)).toEqual(['revert', 'rebase']);
		expect(conflictActions({ kind: 'base_added', resolved_at: null }, true)).toEqual(['revert', 'rebase']);
		expect(conflictActions({ kind: 'base_removed', resolved_at: null }, true)).toEqual(['revert']);
		expect(conflictActions({ kind: 'parent_missing', resolved_at: null }, true)).toEqual(['revert']);
		expect(conflictActions({ kind: 'base_changed', resolved_at: '2026-10-17T00:00:00Z' }, true)).toEqual([]);
		expect(conflictActions({ kind: 'base_changed', resolved_at: null }, false)).toEqual([]);
	});
	it('prints keys in natural-key order', () => {
		expect(
			conflictKeyText({
				object: 'ontology_relation',
				key: { object: 'Order', subject: 'Customer', predicate: 'places' },
			}),
		).toBe('Customer · places · Order');
		expect(conflictKeyText({ object: 'procedure', key: { name: 'return', version: '1.3.0' } })).toBe(
			'return · 1.3.0',
		);
	});
});

describe('context-space override editor', () => {
	const faq = { label: 'General questions', required: ['question'], entities: ['Policy'] };

	it('starts from the current values and sends only what changed', () => {
		const d = spaceDraft(faq);
		expect(d).toEqual({ label: 'General questions', required: 'question', entities: ['Policy'] });
		expect(spacePatch(faq, d)).toEqual({});
		expect(spacePatch(faq, { ...d, label: '  Vragen ' })).toEqual({ label: 'Vragen' });
		expect(spacePatch(faq, { ...d, required: 'question, order_id,, ' })).toEqual({
			required: ['question', 'order_id'],
		});
		expect(spacePatch(faq, { ...d, entities: [] })).toEqual({ entities: [] });
	});

	it('mirrors the server checks for label and required fields', () => {
		expect(spaceFormErrors(spaceDraft(faq))).toEqual([]);
		expect(spaceFormErrors({ label: ' ', required: '', entities: [] })).toEqual(['Label is required.']);
		expect(spaceFormErrors({ label: 'x'.repeat(121), required: '', entities: [] })).toHaveLength(1);
		expect(spaceFormErrors({ label: 'x', required: 'Order Id, ok', entities: [] })[0]).toContain('Order Id');
		expect(spaceFormErrors({ label: 'x', required: 'a, a', entities: [] })).toEqual([
			'Each required field once.',
		]);
		expect(parseList(' a ,b,, c ')).toEqual(['a', 'b', 'c']);
	});

	it('keeps routing fields out of the editable set', () => {
		expect([...SPACE_EDITABLE]).toEqual(['label', 'required', 'entities']);
		for (const f of SPACE_ROUTING) expect(SPACE_EDITABLE as readonly string[]).not.toContain(f);
	});
});
