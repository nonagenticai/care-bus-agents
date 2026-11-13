import { describe, expect, it } from 'vitest';
import { ApiError } from '../../src/lib/api/client';
import { guard, guardOutcome } from '../../src/lib/app.svelte';
import { toastState } from '../../src/lib/ui/toaster.svelte';
import {
	CHECKING_TEXT,
	CONNECTOR_FAULTS,
	NOT_SAVED_TEXT,
	UNREACHABLE_TEXT,
	classifyWriteError,
	compareConnector,
	compareProvider,
	compareRemoved,
	compareSource,
	faultyMappingRows,
	fieldFault,
	orNull,
	reconcile,
	reconcileText,
	sameValue,
} from '../../src/lib/write-outcome';
import { draftOf, inputOf } from '../../src/lib/sources';
import type { Connector, Source } from '../../src/lib/api/types';
import type { ProviderInput, ProviderView } from '../../src/lib/api/providers';
import { toConnectorInput } from '../../src/routes/admin/jobs/[job]/backends/connector-form';

const fail =
	(status: number, code: string, message = 'x') =>
	async () => {
		throw new ApiError(status, code, message);
	};

describe('classifyWriteError', () => {
	it('is uncertain only when no answer from carebus-api came back', () => {
		expect(classifyWriteError(new ApiError(0, 'unavailable', 'down'))).toBe('uncertain');
		expect(classifyWriteError(new ApiError(502, 'http_502', 'Bad Gateway'))).toBe('uncertain');
		expect(classifyWriteError(new ApiError(504, 'http_504', 'Gateway Timeout'))).toBe('uncertain');
		expect(classifyWriteError(new ApiError(200, 'bad_response', 'not JSON'))).toBe('uncertain');
	});
	it('is a definite refusal for answered errors, 503 included, and for non-API errors', () => {
		for (const s of [400, 403, 404, 409, 422, 500, 503])
			expect(classifyWriteError(new ApiError(s, 'c', 'm'))).toBe('rejected');
		expect(classifyWriteError(new Error('boom'))).toBe('rejected');
	});
});

describe('guardOutcome', () => {
	it('reports ok, rejected and uncertain', async () => {
		expect(await guardOutcome(async () => 1, 'Saved.')).toEqual({ kind: 'ok' });
		expect(toastState.msg).toBe('Saved.');
		expect((await guardOutcome(fail(422, 'invalid_cap', 'bad cap'), 'Saved.')).kind).toBe('rejected');
		expect(toastState.msg).toBe('Not saved: bad cap');
		expect((await guardOutcome(fail(403, 'forbidden'), 'Saved.')).kind).toBe('rejected');
		const u = await guardOutcome(fail(0, 'unavailable'), 'Saved.');
		expect(u.kind).toBe('uncertain');
		expect(toastState.msg).toMatch(/^The write outcome is uncertain/);
	});
	it('says a check is running when the caller reconciles', async () => {
		await guardOutcome(fail(504, 'http_504'), 'Saved.', 'Not saved', { reconcile: true });
		expect(toastState.msg).toBe(CHECKING_TEXT);
	});
	it('keeps the boolean guard() working', async () => {
		expect(await guard(async () => 1, 'Saved.')).toBe(true);
		expect(await guard(fail(0, 'unavailable'), 'Saved.')).toBe(false);
		expect(await guard(fail(400, 'invalid_body'), 'Saved.')).toBe(false);
	});
});

describe('reconcile', () => {
	const same = () => ({ differs: [], unchecked: [] });
	it('confirms when the stored resource matches, never calling anything but the read', async () => {
		let reads = 0;
		const r = await reconcile(async () => {
			reads++;
			return { a: 1 };
		}, same);
		expect(r).toEqual({ kind: 'confirmed', unchecked: [] });
		expect(reads).toBe(1);
	});
	it('reports not saved when it differs, and unreachable when the read fails', async () => {
		expect(
			await reconcile(
				async () => null,
				() => ({ differs: ['x'], unchecked: [] }),
			),
		).toEqual({
			kind: 'not_saved',
			differs: ['x'],
		});
		expect(await reconcile(fail(0, 'unavailable'), same)).toEqual({ kind: 'unreachable' });
	});
	it('orNull maps only 404 to null', async () => {
		expect(await orNull(fail(404, 'not_found')())).toBeNull();
		await expect(orNull(fail(0, 'unavailable')())).rejects.toThrow();
	});
	it('words each result, naming unchecked secrets', () => {
		expect(reconcileText({ kind: 'confirmed', unchecked: [] })).toBe('Saved — confirmed after reloading.');
		expect(reconcileText({ kind: 'confirmed', unchecked: ['API key'] })).toMatch(
			/Only the non-secret fields could be checked; the API key is never shown back/,
		);
		expect(reconcileText({ kind: 'not_saved', differs: [] })).toBe(NOT_SAVED_TEXT);
		expect(reconcileText({ kind: 'unreachable' })).toBe(UNREACHABLE_TEXT);
	});
});

describe('sameValue', () => {
	it('treats an empty string and null as the same blank value', () => {
		expect(sameValue('', null)).toBe(true);
		expect(sameValue(0, null)).toBe(false);
	});
});

describe('compareConnector', () => {
	const conn: Connector = {
		id: 'oms',
		name: 'OMS',
		system: 'oms',
		base_url: 'https://oms.example',
		auth_type: 'api_key',
		secret_ref: 'carebus/oms',
		timeout_ms: 5000,
		rate_limit: null,
		origin: 'tenant',
		overridden: false,
		hidden: false,
		health: 'ok',
		health_checked_at: null,
		mappings: [
			{ backend_field: 'b', attribute: 'Order.b' },
			{ backend_field: 'a', attribute: 'Order.a' },
		],
	};
	it('ignores mapping order (the API sorts by backend field) and health columns', () => {
		const stored = { ...conn, health: 'down' as const, mappings: [...conn.mappings].reverse() };
		expect(compareConnector(toConnectorInput(conn), stored).differs).toEqual([]);
	});
	it('flags a changed field or a missing connector', () => {
		expect(compareConnector(toConnectorInput(conn), { ...conn, timeout_ms: 1 }).differs).toEqual([
			'timeout_ms',
		]);
		expect(compareConnector(toConnectorInput(conn), null).differs.length).toBeGreaterThan(0);
	});
});

describe('compareProvider', () => {
	const view: ProviderView = {
		base_url: 'https://p.example/v1',
		dialect: 'openai',
		model: 'm',
		model_id: null,
		dims: 1024,
		regions: ['eu'],
		zdr: true,
		timeout_ms: 1000,
		concurrency: 2,
		has_api_key: true,
		thinking: null,
		temperature: null,
		updated_at: null,
	};
	const sent: ProviderInput = {
		base_url: 'https://p.example/v1',
		dialect: 'openai',
		model: 'm',
		model_id: null,
		dims: 1024,
		regions: ['eu'],
		zdr: true,
		timeout_ms: 1000,
		concurrency: 2,
	};
	it('confirms non-secret fields and marks a sent key as unchecked', () => {
		expect(compareProvider(sent, view)).toEqual({ differs: [], unchecked: [] });
		expect(compareProvider({ ...sent, api_key: 'k' }, view)).toEqual({ differs: [], unchecked: ['API key'] });
	});
	it('a sent key with no key stored, a changed field, or no registration is not saved', () => {
		expect(compareProvider({ ...sent, api_key: 'k' }, { ...view, has_api_key: false }).differs).toEqual([
			'api_key',
		]);
		expect(compareProvider({ ...sent, dims: 768 }, view).differs).toEqual(['dims']);
		expect(compareProvider(sent, null).differs.length).toBeGreaterThan(0);
	});
	it('a removal landed when no registration is left', () => {
		expect(compareRemoved(null).differs).toEqual([]);
		expect(compareRemoved(view).differs).toEqual(['provider']);
	});
	it('checks chat settings after a lost save answer, including resets to provider defaults', () => {
		const { dims: _, ...fields } = sent;
		const chat = { ...fields, thinking: true, temperature: 0.2 };
		const stored = { ...view, dims: null, thinking: true, temperature: 0.2 };
		expect(compareProvider(chat, stored).differs).toEqual([]);
		expect(compareProvider(chat, { ...stored, thinking: false, temperature: 0.7 }).differs).toEqual([
			'thinking',
			'temperature',
		]);
		expect(compareProvider({ ...chat, thinking: null, temperature: null }, stored).differs).toEqual([
			'thinking',
			'temperature',
		]);
	});
});

describe('compareSource', () => {
	const src: Source = {
		id: 's1',
		name: 'Customers',
		type: 'api',
		config: { url: 'https://api.example.nl/v2/customers', auth: { type: 'bearer' } },
		mapping: [{ source_field: 'id', entity: 'Customer', attribute: 'id' }],
		target_store_id: 'store-1',
		schedule: null,
		enabled: true,
		has_credentials: true,
		created_at: '',
		updated_at: '',
		updated_by: 'admin',
		last_run: null,
	};
	const norm = (s: Source) => inputOf(draftOf(s), s.has_credentials);
	it('confirms through the editor round trip and marks a sent secret as unchecked', () => {
		const sent = { ...norm(src), credentials: { secret: 'tok' } };
		expect(compareSource(sent, src, norm)).toEqual({ differs: [], unchecked: ['credentials secret'] });
		expect(compareSource(norm(src), src, norm)).toEqual({ differs: [], unchecked: [] });
	});
	it('flags changes, a cleared secret still stored, and a missing source', () => {
		expect(compareSource({ ...norm(src), enabled: false }, src, norm).differs).toEqual(['enabled']);
		expect(compareSource({ ...norm(src), credentials: null }, src, norm).differs).toEqual(['credentials']);
		expect(
			compareSource({ ...norm(src), credentials: { secret: 't' } }, { ...src, has_credentials: false }, norm)
				.differs,
		).toEqual(['credentials']);
		expect(compareSource(norm(src), null, norm).differs.length).toBeGreaterThan(0);
	});
});

describe('fieldFault', () => {
	const CONN = ['base_url', 'auth_type', 'secret_ref', 'timeout_ms', 'mappings'] as const;
	it('maps a validation code onto its field', () => {
		expect(fieldFault(new ApiError(400, 'invalid_base_url', 'url'), CONNECTOR_FAULTS, CONN)).toEqual({
			field: 'base_url',
			code: 'invalid_base_url',
			message: 'url',
		});
		expect(
			fieldFault(new ApiError(400, 'invalid_timeout', 't'), CONNECTOR_FAULTS, ['timeout_ms']).field,
		).toBe('timeout_ms');
	});
	it('reads the field from a secret rejection, and leaves unknown codes unplaced', () => {
		const e = new ApiError(
			422,
			'secret_value_rejected',
			'field secret_ref looks like a secret; store a secret_ref instead',
		);
		expect(fieldFault(e, {}, CONN).field).toBe('secret_ref');
		expect(fieldFault(new ApiError(400, 'invalid_body', 'b'), CONNECTOR_FAULTS, CONN).field).toBeNull();
		expect(fieldFault(new ApiError(403, 'forbidden', 'f'), CONNECTOR_FAULTS, CONN).code).toBe('forbidden');
	});
	it('points a mapping refusal at the rows it is about', () => {
		const rows = [
			{ backend_field: 'a', attribute: 'Order.a' },
			{ backend_field: 'a', attribute: 'Order.b' },
			{ backend_field: '', attribute: 'Order.c' },
			{ backend_field: 'd', attribute: 'nodot' },
		];
		const bad = fieldFault(new ApiError(400, 'invalid_mapping', 'm'), CONNECTOR_FAULTS, ['mappings']);
		expect(faultyMappingRows(bad, rows)).toEqual([1, 2, 3]);
		const unknown = fieldFault(
			new ApiError(400, 'unknown_attribute', 'the ontology has no attribute Order.b'),
			CONNECTOR_FAULTS,
			['mappings'],
		);
		expect(faultyMappingRows(unknown, rows)).toEqual([1]);
		expect(faultyMappingRows(null, rows)).toEqual([]);
	});
});

describe('round trips of the shipped fixtures', () => {
	it('an unchanged tenant provider re-read after a lost answer is confirmed', async () => {
		const { readFileSync } = await import('node:fs');
		const { draftFor, toProviderInput } = await import('../../src/lib/api/providers');
		const data = JSON.parse(readFileSync('src/lib/api/fixtures/providers/providers.json', 'utf8'));
		const tenant = data.capabilities.filter((c: { source: string }) => c.source === 'tenant');
		expect(tenant.length).toBeGreaterThan(0);
		for (const c of tenant)
			expect(compareProvider(toProviderInput(c, draftFor(c)), c.provider).differs).toEqual([]);
	});
	it('an unchanged source re-read after a lost answer is confirmed', async () => {
		const { readFileSync } = await import('node:fs');
		const s: Source = JSON.parse(readFileSync('src/lib/api/fixtures/source.json', 'utf8'));
		const norm = (x: Source) => inputOf(draftOf(x), x.has_credentials);
		expect(compareSource(norm(s), s, norm)).toEqual({ differs: [], unchecked: [] });
	});
});
