import type { ContextSpaceInput, RefinableObject, RefinementOp, ReleaseConflict } from './api/types';

/**
 * Base vs tenant, side by side (W3 plan 14). Each overlay object is listed twice by carebus-api: `?layer=base` (the
 * job's current release) and `?layer=tenant` (this tenant's rows, tombstones included). A row pairs the two by
 * natural key (refine.rs `ObjectKind::key_columns`).
 */

/** The objects the side-by-side page shows, with their natural key columns. */
export const KEY_COLUMNS = {
	connector: ['id'],
	context_space: ['key'],
	knowledge_store: ['key'],
	ontology_entity: ['name'],
	ontology_relation: ['subject', 'predicate', 'object'],
} as const satisfies Partial<Record<RefinableObject, readonly string[]>>;
export type SideObject = keyof typeof KEY_COLUMNS;

/** `base` = release only; `override` = tenant row over a base row; `extension` = tenant only; `tombstone` = hidden. */
export type RowState = 'base' | 'override' | 'extension' | 'tombstone';
export type FieldDiff = { field: string; base: string; tenant: string };
type Item = Record<string, unknown>;
export type SideRow<T extends Item = Item> = {
	object: SideObject;
	key: Record<string, string>;
	keyText: string;
	base: T | null;
	tenant: T | null;
	state: RowState;
	diff: FieldDiff[];
	conflict: ReleaseConflict | null;
};

/** Not configuration: provenance flags, observations and bookkeeping columns. */
export const IGNORED_FIELDS = new Set([
	'origin',
	'overridden',
	'hidden',
	'shadowed',
	'health',
	'health_checked_at',
	'created_at',
	'updated_at',
	'updated_by',
	'release_id',
	'content_hash',
	'base_hash',
	'tenant_id',
	'job_id',
	'fk_origin',
]);

/** The natural key of `item` as the refinement routes take it. */
export function keyOf(object: SideObject, item: Item): Record<string, string> {
	const out: Record<string, string> = {};
	for (const c of KEY_COLUMNS[object]) out[c] = String(item[c] ?? '');
	return out;
}

export function keyTextOf(object: SideObject, key: Record<string, unknown>): string {
	return KEY_COLUMNS[object].map((c) => String(key[c] ?? '')).join(' · ');
}

/** A value as the page prints it; an unset value reads "not set", never "null". */
export function fmtValue(v: unknown): string {
	if (v === null || v === undefined || v === '') return 'not set';
	if (typeof v === 'string') return v;
	if (Array.isArray(v))
		return v.length ? v.map((x) => (typeof x === 'string' ? x : JSON.stringify(x))).join(', ') : 'none';
	if (typeof v === 'object') return JSON.stringify(v);
	return String(v);
}

/** The configuration fields whose base and tenant values differ, in the base item's field order. */
export function diffFields(base: Item, tenant: Item): FieldDiff[] {
	const fields = [...new Set([...Object.keys(base), ...Object.keys(tenant)])].filter(
		(f) => !IGNORED_FIELDS.has(f),
	);
	return fields
		.filter((f) => JSON.stringify(base[f] ?? null) !== JSON.stringify(tenant[f] ?? null))
		.map((f) => ({ field: f, base: fmtValue(base[f]), tenant: fmtValue(tenant[f]) }));
}

/** Pairs the base and tenant lists by natural key; rows sorted by key. Open conflicts attach to their row. */
export function sideBySide<T extends Item>(
	object: SideObject,
	base: T[],
	tenant: T[],
	openConflicts: ReleaseConflict[] = [],
): SideRow<T>[] {
	const b = new Map(base.map((i) => [keyTextOf(object, i), i]));
	const t = new Map(tenant.map((i) => [keyTextOf(object, i), i]));
	const keys = [...new Set([...b.keys(), ...t.keys()])].sort((x, y) => x.localeCompare(y));
	return keys.map((k) => {
		const bi = b.get(k) ?? null;
		const ti = t.get(k) ?? null;
		const state: RowState = ti ? (ti.hidden === true ? 'tombstone' : bi ? 'override' : 'extension') : 'base';
		const conflict =
			openConflicts.find((c) => c.object === object && !c.resolved_at && keyTextOf(object, c.key) === k) ??
			null;
		return {
			object,
			key: keyOf(object, (ti ?? bi) as Item),
			keyText: k,
			base: bi,
			tenant: ti,
			state,
			diff: state === 'override' && bi && ti ? diffFields(bi, ti) : [],
			conflict,
		};
	});
}

/**
 * What an admin may do with a row: revert an override (and rebase it when the release changed under it), unhide a
 * tombstone, hide a base row. An extension is removed on its own page. Nothing for a read-only caller.
 */
export function actionsFor(row: Pick<SideRow, 'state' | 'conflict'>, canWrite: boolean): RefinementOp[] {
	if (!canWrite) return [];
	switch (row.state) {
		case 'override':
			return row.conflict ? ['revert', 'rebase'] : ['revert'];
		case 'tombstone':
			return ['unhide'];
		case 'base':
			return ['hide'];
		case 'extension':
			return [];
	}
}

export const ACTION_LABEL: Record<RefinementOp, string> = {
	revert: 'Revert to base',
	rebase: 'Keep mine',
	hide: 'Hide',
	unhide: 'Unhide',
};

export const STATE_LABEL: Record<RowState, string> = {
	base: 'From the release',
	override: 'Refined',
	extension: 'Added by you',
	tombstone: 'Hidden',
};

/**
 * Context-space override (routes::ontology::put_space): what a tenant may change on a base space. `key`, `area`,
 * `request_type` and `procedure` route the space (stream names, the qualifier's classification, the plan) and come
 * with the release only.
 */
export const SPACE_EDITABLE = ['label', 'required', 'entities'] as const;
export const SPACE_ROUTING = ['key', 'area', 'request_type', 'procedure'] as const;
export const SPACE_FIELD_LABEL: Record<
	(typeof SPACE_EDITABLE)[number] | (typeof SPACE_ROUTING)[number],
	string
> = {
	label: 'Label',
	required: 'Required fields',
	entities: 'Entities',
	key: 'Key',
	area: 'Area',
	request_type: 'Request type',
	procedure: 'Procedure',
};
const SLOT_RE = /^[a-z][a-z0-9_]{0,63}$/;
export const SPACE_LABEL_MAX = 120;

/** The editor's form state: `required` as the comma-separated text the admin types. */
export type SpaceDraft = { label: string; required: string; entities: string[] };
type SpaceValues = { label: string; required: string[]; entities: string[] };

export function spaceDraft(s: SpaceValues): SpaceDraft {
	return { label: s.label, required: s.required.join(', '), entities: [...s.entities] };
}

/** `a, b ,, c` → `['a', 'b', 'c']`. */
export function parseList(text: string): string[] {
	return text
		.split(',')
		.map((x) => x.trim())
		.filter(Boolean);
}

/** Client-side checks mirroring the server's 400s (the server stays the authority). */
export function spaceFormErrors(d: SpaceDraft): string[] {
	const e: string[] = [];
	const label = d.label.trim();
	if (!label) e.push('Label is required.');
	else if ([...label].length > SPACE_LABEL_MAX) e.push(`Label is at most ${SPACE_LABEL_MAX} characters.`);
	const req = parseList(d.required);
	const bad = req.filter((f) => !SLOT_RE.test(f));
	if (bad.length) e.push(`Required fields are snake_case names (not ${bad.join(', ')}).`);
	if (new Set(req).size !== req.length) e.push('Each required field once.');
	return e;
}

/** Only the fields that differ from `current` (the values in force now); `{}` when nothing changed. */
export function spacePatch(current: SpaceValues, d: SpaceDraft): ContextSpaceInput {
	const out: ContextSpaceInput = {};
	const label = d.label.trim();
	if (label !== current.label) out.label = label;
	const req = parseList(d.required);
	if (JSON.stringify(req) !== JSON.stringify(current.required)) out.required = req;
	if (JSON.stringify(d.entities) !== JSON.stringify(current.entities)) out.entities = [...d.entities];
	return out;
}
