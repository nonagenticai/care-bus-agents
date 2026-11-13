import { describe, expect, it, vi } from 'vitest';
import { ApiError, createClient, looksLikeSecret } from '../../src/lib/api/client';

const json = (status: number, body: unknown) =>
	new Response(JSON.stringify(body), {
		status,
		headers: { 'content-type': 'application/json' },
	});

describe('api client', () => {
	it('asks the token provider on every request and sends a bearer header', async () => {
		const token = vi.fn().mockResolvedValueOnce('t1').mockResolvedValueOnce('t2');
		const fetchFn = vi.fn().mockResolvedValue(json(200, { items: [], next: null }));
		const api = createClient({ baseUrl: 'https://api.test', token, fetch: fetchFn });
		await api.cases({ limit: 10 });
		await api.cases({ limit: 10 });
		expect(token).toHaveBeenCalledTimes(2);
		expect(fetchFn.mock.calls[0]![0]).toBe('https://api.test/api/jobs/customer_care_agent/cases?limit=10');
		expect(fetchFn.mock.calls[1]![1].headers.Authorization).toBe('Bearer t2');
	});

	it('after a 401 forces a token refresh and retries once', async () => {
		const token = vi.fn(async (o?: { force?: boolean }) => (o?.force ? 'fresh' : 'stale'));
		const fetchFn = vi
			.fn()
			.mockResolvedValueOnce(json(401, { error: { code: 'unauthorized', message: 'token expired' } }))
			.mockResolvedValueOnce(json(200, { items: [], next: null }));
		const api = createClient({ baseUrl: 'https://api.test', token, fetch: fetchFn });
		await expect(api.agents()).resolves.toEqual({ items: [], next: null });
		expect(token).toHaveBeenLastCalledWith({ force: true });
		expect(fetchFn.mock.calls[1]![1].headers.Authorization).toBe('Bearer fresh');
	});

	it('a second 401 surfaces as ApiError 401 (no retry loop)', async () => {
		const fetchFn = vi.fn().mockResolvedValue(json(401, { error: { code: 'unauthorized', message: 'no' } }));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await expect(api.agents()).rejects.toMatchObject({ status: 401 });
		expect(fetchFn).toHaveBeenCalledTimes(2);
	});

	it('maps the error envelope to ApiError with code and status', async () => {
		const api = createClient({
			baseUrl: 'https://api.test',
			token: async () => 't',
			fetch: async () => json(403, { error: { code: 'forbidden', message: 'tenant_admin of that tenant' } }),
		});
		await expect(api.replayDeadLetter('1792066211044-0')).rejects.toMatchObject({
			status: 403,
			code: 'forbidden',
		});
	});

	it('maps a network failure to status 0 "unavailable", never to empty data', async () => {
		const api = createClient({
			baseUrl: 'https://api.test',
			token: async () => 't',
			fetch: async () => {
				throw new TypeError('Failed to fetch');
			},
		});
		const err = await api.overview().catch((e) => e);
		expect(err).toBeInstanceOf(ApiError);
		expect(err.status).toBe(0);
		expect(err.code).toBe('unavailable');
	});

	it('encodes path segments such as stream keys', async () => {
		const fetchFn = vi.fn().mockResolvedValue(json(200, { key: 'bus:tool:delivery', groups: [], length: 0 }));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await api.stream('bus:tool:delivery');
		expect(fetchFn.mock.calls[0]![0]).toBe(
			'https://api.test/api/jobs/customer_care_agent/streams/bus%3Atool%3Adelivery',
		);
	});

	it('lists hidden entities only when asked (include_hidden)', async () => {
		const fetchFn = vi.fn().mockResolvedValue(json(200, { items: [], next: null }));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await api.entities({ includeHidden: true });
		await api.entities();
		expect(fetchFn.mock.calls[0]![0]).toBe(
			'https://api.test/api/jobs/customer_care_agent/ontology/entities?include_hidden=true',
		);
		expect(fetchFn.mock.calls[1]![0]).toBe('https://api.test/api/jobs/customer_care_agent/ontology/entities');
	});

	it('pages demo records with offset, as demo-backends does', async () => {
		const fetchFn = vi.fn().mockResolvedValue(
			json(200, {
				table: 'orders',
				primary_key: 'order_id',
				editable: [],
				columns: [],
				rows: [],
				total: 0,
			}),
		);
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await api.demoRecords('orders', { q: 'ORD-58', limit: 50, offset: 100 });
		expect(fetchFn.mock.calls[0]![0]).toBe(
			'https://api.test/api/jobs/customer_care_agent/demo/records/orders?q=ORD-58&limit=50&offset=100',
		);
	});

	it('sends procedure moves as {to} and refuses a secret value in secret_ref', async () => {
		const fetchFn = vi.fn().mockResolvedValue(json(200, {}));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await api.setProcedureStatus('return', '1.4.0-beta', 'CANARY');
		expect(JSON.parse(fetchFn.mock.calls[0]![1].body)).toEqual({ to: 'CANARY' });
		await api.approveProcedure('return', '1.4.0-beta');
		expect(fetchFn.mock.calls[1]![0]).toBe(
			'https://api.test/api/jobs/customer_care_agent/procedures/return/versions/1.4.0-beta/approve',
		);
		expect(fetchFn.mock.calls[1]![1].method).toBe('POST');
		await expect(
			api.saveConnector({ id: 'oms', name: 'OMS', secret_ref: 'sk-ant-abc123' } as never),
		).rejects.toThrow(/secret_ref must name a secret/);
		expect(fetchFn).toHaveBeenCalledTimes(2);
	});

	it('accepts a long slash-separated secret ref but rejects secret-looking values', async () => {
		const fetchFn = vi.fn().mockResolvedValue(json(200, {}));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		const conn = { id: 'oms', name: 'OMS' };
		const long = 'carebus-connectors/oms-api-key-production-eu-west-01';
		expect(long.length).toBeGreaterThanOrEqual(40);
		await api.saveConnector({ ...conn, secret_ref: long } as never);
		expect(fetchFn).toHaveBeenCalledTimes(1);

		const mixed = 'aB3dE6gH9jK2mN5pQ8sT1vW4yZ7bC0eF3hJ6kM9n';
		for (const bad of ['sk-ant-abc123', 'eyJhbGciOiJIUzI1NiJ9', '-----BEGIN PRIVATE KEY-----', mixed]) {
			await expect(api.saveConnector({ ...conn, secret_ref: bad } as never)).rejects.toMatchObject({
				status: 400,
				code: 'secret_value',
			});
		}
		expect(fetchFn).toHaveBeenCalledTimes(1);
	});

	it('looksLikeSecret ports plan 05 exactly', () => {
		expect(looksLikeSecret('carebus-llm/anthropic-api-key')).toBe(false);
		expect(looksLikeSecret('a'.repeat(60))).toBe(false); // one character class
		expect(looksLikeSecret('Aa1'.repeat(14))).toBe(true); // 42 chars, three classes
		expect(looksLikeSecret(`${'Aa1'.repeat(14)}.x`)).toBe(false); // '.' excludes
		expect(looksLikeSecret(`  sk-live  `)).toBe(true); // trimmed, sk- prefix
	});

	it('a 2xx non-JSON body (an index.html from the wrong host) is a bad_response error, not null', async () => {
		const api = createClient({
			baseUrl: 'https://api.test',
			token: async () => 't',
			fetch: async () =>
				new Response('<!doctype html><html></html>', {
					status: 200,
					headers: { 'content-type': 'text/html' },
				}),
		});
		await expect(api.agents()).rejects.toMatchObject({
			status: 200,
			code: 'bad_response',
			message: 'carebus-api returned a non-JSON response',
		});
	});

	it('a 200 with an empty body still resolves for a write that returns nothing', async () => {
		const api = createClient({
			baseUrl: 'https://api.test',
			token: async () => 't',
			fetch: async () => new Response('', { status: 200 }),
		});
		await expect(api.discardDeadLetter('1-0')).resolves.toBeUndefined();
	});
});

describe('sources client', () => {
	const ok = (body: unknown, status = 200) =>
		new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

	it('createSource sends JSON with credentials; a secret in config fails before any request', async () => {
		const fetchFn = vi.fn().mockResolvedValue(ok({ id: 's1' }, 201));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		// Built at runtime so no high-entropy literal is committed.
		const secret = 'x'.repeat(8) + '-' + Date.now();
		await api.createSource({
			name: 'Customer API',
			type: 'api',
			config: { url: 'https://api.example.nl/c', auth: { type: 'bearer' } },
			mapping: [],
			target_store_id: 'leaf',
			schedule: null,
			enabled: true,
			credentials: { secret },
		});
		const [, init] = fetchFn.mock.calls[0]!;
		expect(init.headers['Content-Type']).toBe('application/json');
		expect(JSON.parse(init.body).credentials.secret).toBe(secret);
		await expect(
			api.saveSource('s1', {
				name: 'Customer API',
				type: 'api',
				config: { url: 'https://api.example.nl/c', note: 'sk-live-abc' },
				mapping: [],
				target_store_id: 'leaf',
				schedule: null,
				enabled: true,
			}),
		).rejects.toMatchObject({ code: 'secret_value' });
		expect(fetchFn).toHaveBeenCalledTimes(1);
	});

	it('uploads send FormData without a Content-Type; delete passes purge', async () => {
		const fetchFn = vi.fn().mockResolvedValue(ok({ columns: [] }));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await api.previewColumns(new File(['a,b\n1,2\n'], 'x.csv', { type: 'text/csv' }));
		const [url, init] = fetchFn.mock.calls[0]!;
		expect(url).toBe('https://api.test/api/jobs/customer_care_agent/sources/preview-columns');
		expect(init.body).toBeInstanceOf(FormData);
		expect((init.body as FormData).get('file')).toBeInstanceOf(File);
		expect(init.headers['Content-Type']).toBeUndefined();
		expect((init.body as FormData).has('sheet')).toBe(false);
		await api.previewColumns(new File(['PK'], 'x.xlsx'), ' Customers ');
		expect((fetchFn.mock.calls[1]![1].body as FormData).get('sheet')).toBe('Customers');
		expect((fetchFn.mock.calls[1]![1].body as FormData).has('header_row')).toBe(false);
		await api.previewColumns(new File(['1,2\n'], 'x.csv'), '', false);
		expect((fetchFn.mock.calls[2]![1].body as FormData).get('header_row')).toBe('false');
		await api.previewColumns(new File(['a,b\n'], 'x.csv'), '', true);
		expect((fetchFn.mock.calls[3]![1].body as FormData).get('header_row')).toBe('true');
		fetchFn.mockClear();
		await api.resync('s1', [new File(['%PDF'], 'a.pdf')]);
		expect((fetchFn.mock.calls[0]![1].body as FormData).getAll('file')).toHaveLength(1);
		await api.resync('s2');
		expect(fetchFn.mock.calls[1]![1].body).toBeUndefined();
		await api.deleteSource('s1', true);
		expect(fetchFn.mock.calls[2]![0]).toBe(
			'https://api.test/api/jobs/customer_care_agent/sources/s1?purge=true',
		);
		expect(fetchFn.mock.calls[2]![1].method).toBe('DELETE');
	});

	it('storeDocuments pages with the before cursor', async () => {
		const fetchFn = vi.fn().mockResolvedValue(ok({ items: [], next: null }));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await api.storeDocuments('st 1');
		await api.storeDocuments('st 1', 'cur/1');
		expect(fetchFn.mock.calls[0]![0]).toBe('https://api.test/api/knowledge/stores/st%201/documents');
		expect(fetchFn.mock.calls[1]![0]).toBe(
			'https://api.test/api/knowledge/stores/st%201/documents?before=cur%2F1',
		);
	});

	describe('scope: job prefix and active tenant (W3 plan 10 Task 9)', () => {
		const TENANT = '7f1c9a2e-0000-4000-8000-00000000fe12';
		it('job-scoped routes go under /api/jobs/{job}; the job is read per request', async () => {
			const fetchFn = vi.fn().mockResolvedValue(json(200, { items: [], next: null }));
			let job = 'customer_care_agent';
			const api = createClient({
				baseUrl: 'https://api.test',
				token: async () => 't',
				job: () => job,
				fetch: fetchFn,
			});
			await api.procedures();
			job = 'billing desk';
			await api.procedures();
			expect(fetchFn.mock.calls[0]![0]).toBe('https://api.test/api/jobs/customer_care_agent/procedures');
			expect(fetchFn.mock.calls[1]![0]).toBe('https://api.test/api/jobs/billing%20desk/procedures');
		});
		it('tenant routes (knowledge, providers, ingest, audit, me, jobs) stay at /api/…', async () => {
			const fetchFn = vi.fn().mockResolvedValue(json(200, { items: [], next: null }));
			const api = createClient({
				baseUrl: 'https://api.test',
				token: async () => 't',
				job: () => 'j1',
				fetch: fetchFn,
			});
			await api.stores();
			await api.query({ query: 'q', route_mode: 'embed', top_k: 5, beam: 1, min_score: 0 });
			await api.llmProviders();
			await api.providerSlots();
			await api.ingestJobs();
			await api.audit({});
			await api.me();
			await api.jobs();
			expect(fetchFn.mock.calls.map((c) => c[0])).toEqual([
				'https://api.test/api/knowledge/stores',
				'https://api.test/api/knowledge/query',
				'https://api.test/api/providers',
				'https://api.test/api/providers/slots',
				'https://api.test/api/ingest/jobs',
				'https://api.test/api/audit',
				'https://api.test/api/me',
				'https://api.test/api/jobs',
			]);
		});
		it('sends x-active-tenant-id only when a tenant is active', async () => {
			const fetchFn = vi.fn().mockResolvedValue(json(200, { items: [], next: null }));
			let tenant: string | null = null;
			const api = createClient({
				baseUrl: 'https://api.test',
				token: async () => 't',
				activeTenant: () => tenant,
				fetch: fetchFn,
			});
			await api.procedures();
			tenant = TENANT;
			await api.procedures();
			await api.stores();
			expect(fetchFn.mock.calls[0]![1].headers).not.toHaveProperty('x-active-tenant-id');
			expect(fetchFn.mock.calls[1]![1].headers['x-active-tenant-id']).toBe(TENANT);
			expect(fetchFn.mock.calls[2]![1].headers['x-active-tenant-id']).toBe(TENANT);
			expect(api.scopeHeaders()).toEqual({ 'x-active-tenant-id': TENANT });
		});
		it('the console event stream is job-scoped', () => {
			const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', job: () => 'j1' });
			expect(api.eventsUrl('conv 1')).toBe('https://api.test/api/jobs/j1/events?conversation_id=conv+1');
		});
	});
});

describe('saveContextSpace', () => {
	it('PUTs only the patch to the job-scoped space, key encoded, and returns the effective space', async () => {
		const space = { key: 'retail:faq:general', label: 'Vragen', origin: 'tenant', overridden: true };
		const fetchFn = vi.fn().mockResolvedValue(json(200, space));
		const api = createClient({
			baseUrl: 'https://api.test',
			token: async () => 't',
			job: () => 'j1',
			fetch: fetchFn,
		});
		await expect(
			api.saveContextSpace('retail:faq:general', { label: 'Vragen', required: ['question'] }),
		).resolves.toEqual(space);
		const [url, init] = fetchFn.mock.calls[0]!;
		expect(url).toBe('https://api.test/api/jobs/j1/context-spaces/retail%3Afaq%3Ageneral');
		expect(init.method).toBe('PUT');
		expect(JSON.parse(init.body)).toEqual({ label: 'Vragen', required: ['question'] });
	});

	it('surfaces the server refusal of a routing field as ApiError 400', async () => {
		const fetchFn = vi
			.fn()
			.mockResolvedValue(json(400, { error: { code: 'invalid_body', message: 'come with the release' } }));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await expect(api.saveContextSpace('retail:faq:general', { label: 'x' })).rejects.toMatchObject({
			status: 400,
			code: 'invalid_body',
		});
	});
});

describe('eval and ingest job client', () => {
	const ok = (body: unknown, status = 200) =>
		new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });

	it('imports a CSV as text/csv whatever type the browser gave the file', async () => {
		const fetchFn = vi.fn().mockResolvedValue(ok({ added: 1, total: 1 }, 201));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		const csv = 'query,expected_text\nWhere is my parcel?,tracking\n';
		await expect(
			api.importEvalQuestions(new File([csv], 'q.csv', { type: 'application/vnd.ms-excel' })),
		).resolves.toEqual({ added: 1, total: 1 });
		const [url, init] = fetchFn.mock.calls[0]!;
		expect(url).toBe('https://api.test/api/knowledge/eval/questions/import');
		expect(init.method).toBe('POST');
		expect(init.headers['Content-Type']).toBe('text/csv');
		expect(init.body).toBeInstanceOf(Blob);
		expect(await (init.body as Blob).text()).toBe(csv);
	});

	it('keeps strata_rag eval codes on the ApiError', async () => {
		const fetchFn = vi
			.fn()
			.mockResolvedValue(ok({ error: { code: 'eval_not_ready', message: 'no questions' } }, 409));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await expect(api.runEval({ modes: ['embed'] })).rejects.toMatchObject({
			status: 409,
			code: 'eval_not_ready',
		});
		expect(fetchFn.mock.calls[0]![0]).toBe('https://api.test/api/knowledge/eval/runs');
		expect(JSON.parse(fetchFn.mock.calls[0]![1].body)).toEqual({ modes: ['embed'] });
	});

	it('pages runs with before, PATCHes a question, cancels with POST and deletes with DELETE', async () => {
		const fetchFn = vi.fn().mockResolvedValue(ok({ items: [], next: null }));
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		await api.evalRuns({ before: 'c/1' });
		await api.updateEvalQuestion('q 1', { expected_leaf: null });
		await api.cancelJob('j1');
		await api.deleteJob('j1');
		const calls = fetchFn.mock.calls.map(([u, i]) => [i.method, u]);
		expect(calls).toEqual([
			['GET', 'https://api.test/api/knowledge/eval/runs?before=c%2F1'],
			['PATCH', 'https://api.test/api/knowledge/eval/questions/q%201'],
			['POST', 'https://api.test/api/ingest/jobs/j1/cancel'],
			['DELETE', 'https://api.test/api/ingest/jobs/j1'],
		]);
		expect(JSON.parse(fetchFn.mock.calls[1]![1].body)).toEqual({ expected_leaf: null });
	});
});
