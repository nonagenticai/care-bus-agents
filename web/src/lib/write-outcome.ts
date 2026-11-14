// What happened to a write, and how a form finds out when the answer was lost.
//
// A write either succeeded (`ok`), was refused (`rejected`: the server answered with an error, nothing was
// stored), or its outcome is unknown (`uncertain`: no usable answer came back, so the server may or may not have
// stored it). An uncertain write is never resubmitted automatically: the form re-reads the resource and compares
// it with what it sent (`reconcile`). Pure functions only, so they are unit tested directly.
import { ApiError } from './api/client';
import type { Connector, ConnectorInput, Mapping, Source, SourceInput } from './api/types';
import type { ProviderInput, ProviderView } from './api/providers';

export type WriteKind = 'ok' | 'rejected' | 'uncertain';
export type WriteOutcome =
	{ kind: 'ok' } | { kind: 'rejected'; error: unknown } | { kind: 'uncertain'; error: unknown };

/**
 * A failed write is uncertain when no answer from carebus-api itself came back: the network failed (status 0),
 * a gateway gave up (502, 504), or the body was not ours (`bad_response`). Everything else, 503 included, is a
 * definite refusal.
 */
export function classifyWriteError(e: unknown): 'rejected' | 'uncertain' {
	return e instanceof ApiError &&
		(e.status === 0 || e.status === 502 || e.status === 504 || e.code === 'bad_response')
		? 'uncertain'
		: 'rejected';
}

/** The result of re-reading a resource after an uncertain write. */
export type Reconciliation =
	| { kind: 'checking' }
	/** Stored as sent. `unchecked` names write-only fields the API never echoes, so they could not be compared. */
	| { kind: 'confirmed'; unchecked: string[] }
	/** The stored resource differs from what was sent (or does not exist): the write did not land. */
	| { kind: 'not_saved'; differs: string[] }
	/** The re-read failed too. */
	| { kind: 'unreachable' };

export interface Comparison {
	/** Fields whose stored value is not the sent one. Empty = the write landed. */
	differs: string[];
	/** Write-only fields that could not be compared. */
	unchecked: string[];
}

export const CHECKING_TEXT = 'No answer came back. Checking whether the change was saved…';
export const NOT_SAVED_TEXT = 'Not saved — your changes are still here; you can save again.';
export const UNREACHABLE_TEXT = "Still can't reach the server; your changes are kept.";

export function reconcileText(r: Reconciliation): string {
	switch (r.kind) {
		case 'checking':
			return CHECKING_TEXT;
		case 'confirmed':
			return r.unchecked.length
				? `Saved — confirmed after reloading. Only the non-secret fields could be checked; the ${r.unchecked.join(' and ')} is never shown back.`
				: 'Saved — confirmed after reloading.';
		case 'not_saved':
			return NOT_SAVED_TEXT;
		case 'unreachable':
			return UNREACHABLE_TEXT;
	}
}

/** Resolves to null when the resource does not exist (404); any other failure propagates. */
export async function orNull<T>(p: Promise<T>): Promise<T | null> {
	try {
		return await p;
	} catch (e) {
		if (e instanceof ApiError && e.status === 404) return null;
		throw e;
	}
}

/** Re-reads the resource and compares it with what was sent. Never writes. */
export async function reconcile<S>(
	fetchStored: () => Promise<S | null>,
	compare: (stored: S | null) => Comparison,
): Promise<Reconciliation> {
	let stored: S | null;
	try {
		stored = await fetchStored();
	} catch {
		return { kind: 'unreachable' };
	}
	const c = compare(stored);
	return c.differs.length
		? { kind: 'not_saved', differs: c.differs }
		: { kind: 'confirmed', unchecked: c.unchecked };
}

/** JSON with object keys sorted, so key order never makes two equal values differ. */
function canon(v: unknown): string {
	if (v === undefined) return 'null';
	if (v === null || typeof v !== 'object') return JSON.stringify(v);
	if (Array.isArray(v)) return `[${v.map(canon).join(',')}]`;
	const o = v as Record<string, unknown>;
	return `{${Object.keys(o)
		.sort()
		.map((k) => `${JSON.stringify(k)}:${canon(o[k])}`)
		.join(',')}}`;
}

/** `''` and `null` are the same "no value" in these forms. */
const blank = (v: unknown) => (v === '' || v === undefined ? null : v);

export function sameValue(a: unknown, b: unknown): boolean {
	return canon(blank(a)) === canon(blank(b));
}

/** The listed fields whose stored value differs from the sent one; every field when nothing is stored. */
export function compareFields<K extends string>(
	sent: Partial<Record<K, unknown>>,
	stored: Partial<Record<K, unknown>> | null,
	fields: readonly K[],
): string[] {
	if (!stored) return [...fields];
	return fields.filter((f) => !sameValue(sent[f], stored[f]));
}

const CONNECTOR_FIELDS = [
	'name',
	'system',
	'base_url',
	'auth_type',
	'secret_ref',
	'timeout_ms',
	'rate_limit',
	'mappings',
] as const;

/** The API returns mappings ordered by backend field, whatever order they were sent in. */
const byField = (ms: Mapping[]) => [...ms].sort((a, b) => a.backend_field.localeCompare(b.backend_field));

export function compareConnector(sent: ConnectorInput, stored: Connector | null): Comparison {
	const differs = compareFields(
		{ ...sent, mappings: byField(sent.mappings) },
		stored && { ...stored, mappings: byField(stored.mappings) },
		CONNECTOR_FIELDS,
	);
	return { differs, unchecked: [] };
}

const PROVIDER_FIELDS = [
	'base_url',
	'dialect',
	'model',
	'model_id',
	'regions',
	'zdr',
	'timeout_ms',
	'concurrency',
] as const;

/** A provider registration. The API key is write-only: only that one is stored can be checked, not its value. */
export function compareProvider(sent: ProviderInput, stored: ProviderView | null): Comparison {
	const differs = compareFields(sent, stored, PROVIDER_FIELDS);
	for (const field of ['dims', 'thinking', 'temperature'] as const) {
		if (stored && field in sent && !sameValue(sent[field], stored[field])) differs.push(field);
	}
	const unchecked: string[] = [];
	if (sent.api_key !== undefined) {
		if (!stored?.has_api_key) differs.push('api_key');
		unchecked.push('API key');
	}
	return { differs: stored ? differs : [...PROVIDER_FIELDS], unchecked };
}

/** A removal landed when no tenant registration is left. */
export function compareRemoved(stored: ProviderView | null): Comparison {
	return { differs: stored ? ['provider'] : [], unchecked: [] };
}

const SOURCE_FIELDS = [
	'name',
	'type',
	'config',
	'mapping',
	'target_store_id',
	'schedule',
	'enabled',
] as const;

/**
 * A source as stored vs as sent. `normalize` turns the stored source into the body the form would send for it
 * (the editor's own round trip), so defaults the form fills in never read as a difference. Credentials are
 * write-only: whether some are stored is checked, their value cannot be.
 */
export function compareSource(
	sent: SourceInput,
	stored: Source | null,
	normalize: (s: Source) => SourceInput,
): Comparison {
	if (!stored) return { differs: [...SOURCE_FIELDS], unchecked: [] };
	const differs = compareFields(sent, normalize(stored), SOURCE_FIELDS);
	const unchecked: string[] = [];
	if (sent.credentials === null && stored.has_credentials) differs.push('credentials');
	if (sent.credentials) {
		if (!stored.has_credentials) differs.push('credentials');
		unchecked.push('credentials secret');
	}
	return { differs, unchecked };
}

// ---- field-level refusals --------------------------------------------------------------------------------------

export interface FieldFault<F extends string> {
	/** The form field the refusal is about, or null when it names none (shown above the actions). */
	field: F | null;
	code: string;
	message: string;
}

/** carebus-api `connectors.rs` 400 codes → the connector form's fields. */
export const CONNECTOR_FAULTS = {
	invalid_secret_ref: 'secret_ref',
	invalid_timeout: 'timeout_ms',
	invalid_base_url: 'base_url',
	invalid_auth_type: 'auth_type',
	invalid_mapping: 'mappings',
	unknown_attribute: 'mappings',
} as const;
export type ConnectorFaultField = 'base_url' | 'auth_type' | 'secret_ref' | 'timeout_ms' | 'mappings';

/**
 * The refusal as a field fault. The error envelope has no field name, so the machine code picks the field; a
 * `secret_value_rejected` names it in its message ("field X looks like a secret…").
 */
export function fieldFault<F extends string>(
	e: unknown,
	codes: Partial<Record<string, F>>,
	fields: readonly F[],
): FieldFault<F> {
	if (!(e instanceof ApiError))
		return { field: null, code: 'error', message: (e as Error)?.message ?? String(e) };
	if (e.status === 403) return { field: null, code: 'forbidden', message: 'Needs the tenant_admin role.' };
	let field: F | null = codes[e.code] ?? null;
	if (!field && e.code === 'secret_value_rejected') {
		const named = /field (\w+) looks like a secret/.exec(e.message)?.[1];
		field = fields.find((f) => f === named) ?? null;
	}
	return { field, code: e.code, message: e.message };
}

/**
 * Which mapping rows a mapping refusal is about: for `unknown_attribute` the rows with the attribute the message
 * names; for `invalid_mapping` the rows with an empty or repeated backend field, or an attribute without a dot.
 */
export function faultyMappingRows(fault: FieldFault<string> | null, mappings: Mapping[]): number[] {
	if (!fault || fault.field !== 'mappings') return [];
	if (fault.code === 'unknown_attribute') {
		const named = /attribute (\S+)/.exec(fault.message)?.[1];
		return mappings.flatMap((m, i) => (named && m.attribute === named ? [i] : []));
	}
	return mappings.flatMap((m, i) => {
		const f = m.backend_field.trim();
		const repeated = f !== '' && mappings.slice(0, i).some((p) => p.backend_field === m.backend_field);
		return f === '' || repeated || !m.attribute.includes('.') ? [i] : [];
	});
}
