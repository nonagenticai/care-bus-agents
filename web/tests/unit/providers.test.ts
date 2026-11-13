// The providers fixtures are written by crates/carebus-api/tests/providers.rs from the real route; they live in
// fixtures/providers/ and are held here to the key maps in src/lib/api/providers.ts (themselves checked against the
// types at compile time). The form helpers are tested with inline inputs.
import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import {
	PROVIDER_SHAPES,
	PROVIDER_VALUES,
	draftFor,
	errorField,
	keyRequired,
	sourceLabel,
	toProviderInput,
	validateDraft,
	LLM_KINDS,
	LLM_SHAPES,
	LLM_USES,
	USE_RULES,
	LAYA_MODEL_SUGGESTIONS,
	baseUrlProblem,
	bindingOf,
	draftTestProblem,
	formModelChoices,
	llmDraftFor,
	modelChoices,
	providersFor,
	testLine,
	toLlmInput,
	toTestInput,
	unboundNotice,
	useForInitial,
	usesForKind,
	useForPlan,
	useSummary,
	usesOf,
	validateLlmDraft,
	type LlmProvider,
	type LlmProviders,
	type ProviderCapability,
} from '../../src/lib/api/providers';
import { createClient } from '../../src/lib/api/client';

const read = (name: string) =>
	JSON.parse(readFileSync(`src/lib/api/fixtures/providers/${name}.json`, 'utf8'));
const keys = (o: unknown) => Object.keys(o as object).sort();
const mapKeys = (m: object) => Object.keys(m).sort();

describe('providers fixtures match providers.ts', () => {
	const body = read('providers');

	it('has the root, service and capability keys', () => {
		expect(keys(body)).toEqual(mapKeys(PROVIDER_SHAPES.ROOT));
		for (const s of body.services) {
			expect(keys(s)).toEqual(mapKeys(PROVIDER_SHAPES.SERVICE));
			expect(PROVIDER_VALUES.status).toContain(s.status);
			expect(PROVIDER_VALUES.owner).toContain(s.name);
		}
		for (const c of body.capabilities) {
			expect(keys(c), c.capability).toEqual(mapKeys(PROVIDER_SHAPES.CAPABILITY));
			expect(PROVIDER_VALUES.owner).toContain(c.owner);
			expect(PROVIDER_VALUES.source).toContain(c.source);
			if (c.provider) expect(keys(c.provider), c.capability).toEqual(mapKeys(PROVIDER_SHAPES.VIEW));
			if (c.default) expect(keys(c.default), c.capability).toEqual(mapKeys(PROVIDER_SHAPES.DEFAULT));
		}
	});

	it('carries a sample of each nested shape, and no key field anywhere', () => {
		const caps: ProviderCapability[] = body.capabilities;
		expect(caps.some((c) => c.provider)).toBe(true);
		expect(caps.some((c) => c.default)).toBe(true);
		expect(JSON.stringify(body)).not.toContain('"api_key"');
	});

	it('a default rerank has dims null and an ingest registration has dims null', () => {
		const by = (n: string) => body.capabilities.find((c: ProviderCapability) => c.capability === n);
		expect(by('rerank').default).toEqual({ dialect: 'cohere', model: 'BAAI/bge-reranker-v2-m3', dims: null });
		expect(by('ocr').provider.dims).toBeNull();
		expect(by('ocr').provider.dialect).toBe('mistral');
		expect(by('embed').provider.dims).toBe(1024);
	});

	it("chat is strata_rag's, and the chat-only settings are null on every other registration", () => {
		const by = (n: string) => body.capabilities.find((c: ProviderCapability) => c.capability === n);
		expect(by('chat').owner).toBe('strata_rag');
		expect(by('chat').default).toEqual({ dialect: 'openai', model: 'local-llm', dims: null });
		expect(by('embed').provider.thinking).toBeNull();
		expect(by('embed').provider.temperature).toBeNull();
		const order = body.capabilities.map((c: ProviderCapability) => c.capability);
		expect(order).toEqual(['embed', 'rerank', 'chat', 'ocr', 'vision', 'transcribe']);
	});

	it('the error fixture is the standard envelope', () => {
		const e = read('provider_error');
		expect(keys(e)).toEqual(['error']);
		expect(keys(e.error)).toEqual(['code', 'message']);
		expect(errorField(e.error.code)).toBe('regions');
	});
});

const tenant: ProviderCapability = {
	capability: 'embed',
	owner: 'strata_rag',
	available: true,
	source: 'tenant',
	provider: {
		base_url: 'https://embed.example.eu/v1',
		dialect: 'openai',
		model: 'acme-embed',
		model_id: null,
		dims: 1024,
		regions: ['eu'],
		zdr: true,
		timeout_ms: 30000,
		concurrency: 8,
		has_api_key: true,
		thinking: null,
		temperature: null,
		updated_at: '2026-10-16T09:00:00Z',
	},
	default: null,
};
const unconfigured = (capability: string, owner: 'strata_rag' | 'strata_ingest'): ProviderCapability => ({
	capability,
	owner,
	available: true,
	source: 'unconfigured',
	provider: null,
	default: null,
});

describe('provider form', () => {
	it('keeps the stored key for an unchanged base_url and never sends a blank key', () => {
		const d = draftFor(tenant);
		expect(d.api_key).toBe('');
		expect(keyRequired(tenant, d)).toBe(false);
		expect(validateDraft(tenant, d)).toEqual({});
		const input = toProviderInput(tenant, { ...d, api_key: '   ' });
		expect(input).not.toHaveProperty('api_key');
		expect(input.dims).toBe(1024);
	});

	it('embed has no chat settings and only the dialect strata_rag implements', () => {
		const input = toProviderInput(tenant, draftFor(tenant));
		expect(input).not.toHaveProperty('thinking');
		expect(input).not.toHaveProperty('temperature');
		expect(validateDraft(tenant, { ...draftFor(tenant), dialect: 'cohere' }).dialect).toBe('Use openai.');
		const rerank = unconfigured('rerank', 'strata_rag');
		expect(draftFor(rerank).dialect).toBe('cohere');
		expect(validateDraft(rerank, { ...draftFor(rerank), dialect: 'openai' }).dialect).toBe('Use cohere.');
	});

	it('chat: openai, timeout capped at 60000, no dims, thinking and temperature null until set', () => {
		const chat = unconfigured('chat', 'strata_rag');
		const d = draftFor(chat);
		expect(d.dialect).toBe('openai');
		expect(d.timeout_ms).toBe(60000);
		expect(d.dims).toBeNull();
		expect(d.thinking).toBeNull();
		expect(d.temperature).toBeNull();
		const filled = { ...d, base_url: 'https://chat.example.eu/v1', model: 'acme-chat', api_key: 'k' };
		expect(validateDraft(chat, filled)).toEqual({});
		expect(validateDraft(chat, { ...filled, timeout_ms: 60001 }).timeout_ms).toBe('1 to 60000 ms.');
		expect(validateDraft(chat, { ...filled, temperature: 2.5 }).temperature).toBe('0 to 2.');
		expect(validateDraft(chat, { ...filled, temperature: -0.1 }).temperature).toBe('0 to 2.');
		const untouched = toProviderInput(chat, filled);
		expect(untouched).not.toHaveProperty('dims');
		expect(untouched.thinking).toBeNull();
		expect(untouched.temperature).toBeNull();
		const set = toProviderInput(chat, { ...filled, thinking: false, temperature: 0 });
		expect(set.thinking).toBe(false);
		expect(set.temperature).toBe(0);
		// an emptied number input (undefined or NaN) is "not sent", not a value
		const emptied = toProviderInput(chat, { ...filled, temperature: Number.NaN });
		expect(emptied.temperature).toBeNull();
		expect(validateDraft(chat, { ...filled, temperature: Number.NaN })).toEqual({});
	});

	it('a stored chat registration fills the form with its thinking and temperature', () => {
		const stored: ProviderCapability = {
			...tenant,
			capability: 'chat',
			provider: {
				...tenant.provider!,
				base_url: 'https://chat.example.eu/v1',
				dims: null,
				timeout_ms: 60000,
				thinking: true,
				temperature: 0.3,
			},
		};
		const d = draftFor(stored);
		expect([d.thinking, d.temperature, d.dims]).toEqual([true, 0.3, null]);
		expect(keyRequired(stored, d)).toBe(false);
		expect(toProviderInput(stored, d)).toMatchObject({ thinking: true, temperature: 0.3 });
	});

	it('a new base_url needs its own key', () => {
		const d = { ...draftFor(tenant), base_url: 'https://other.example.eu/v1' };
		expect(keyRequired(tenant, d)).toBe(true);
		expect(validateDraft(tenant, d).api_key).toMatch(/new base URL/);
	});

	it('an ingest capability: dialect fixed, no dims, a key required, timeout capped per capability', () => {
		const ocr = unconfigured('ocr', 'strata_ingest');
		const d = draftFor(ocr);
		expect(d.dialect).toBe('mistral');
		expect(d.timeout_ms).toBe(60000);
		expect(d.concurrency).toBe(4);
		const e = validateDraft(ocr, {
			...d,
			base_url: 'https://ocr.example.eu/v1',
			model: 'm',
			timeout_ms: 300001,
		});
		expect(e.api_key).toBe('Enter the API key.');
		expect(e.timeout_ms).toBe('1 to 300000 ms.');
		const vision = unconfigured('vision', 'strata_ingest');
		expect(validateDraft(vision, { ...draftFor(vision), timeout_ms: 110001 }).timeout_ms).toBe(
			'1 to 110000 ms.',
		);
		expect(validateDraft(vision, { ...draftFor(vision), concurrency: 65 }).concurrency).toBe('1 to 64.');
		const input = toProviderInput(ocr, {
			...d,
			base_url: 'https://o.example.eu/v1',
			model: 'm',
			api_key: 'k',
		});
		expect(input).not.toHaveProperty('dims');
		expect(input.api_key).toBe('k');
	});

	it('refuses non-https, a query string, and embed dims other than 1024', () => {
		const d = draftFor(tenant);
		expect(validateDraft(tenant, { ...d, base_url: 'http://embed.example.eu/v1' }).base_url).toBeTruthy();
		expect(
			validateDraft(tenant, { ...d, base_url: 'https://embed.example.eu/v1?x=1' }).base_url,
		).toBeTruthy();
		expect(validateDraft(tenant, { ...d, dims: 768 }).dims).toBe('Must be 1024.');
	});

	it('an unavailable owner reads as unavailable, not unconfigured', () => {
		expect(
			sourceLabel({ ...unconfigured('ocr', 'strata_ingest'), available: false, source: null }).word,
		).toBe('unavailable');
		expect(sourceLabel(unconfigured('vision', 'strata_ingest')).label).toBe('Not configured');
	});

	it('maps owner codes to the field they are about', () => {
		expect(errorField('provider_host_not_allowed')).toBe('base_url');
		expect(errorField('api_key_required')).toBe('api_key');
		expect(errorField('probe_failed')).toBeNull();
	});
});

describe('providers client', () => {
	it('PUTs the capability with the key and passes the owner error code through', async () => {
		const calls: { url: string; init: RequestInit }[] = [];
		const fetch = (async (url: string, init: RequestInit) => {
			calls.push({ url, init });
			return new Response(JSON.stringify({ error: { code: 'probe_failed', message: 'auth' } }), {
				status: 422,
			});
		}) as unknown as typeof globalThis.fetch;
		const c = createClient({ baseUrl: 'http://api', token: async () => 't', fetch });
		const input = toProviderInput(tenant, { ...draftFor(tenant), api_key: 'k-1' });
		await expect(c.saveProviderSlot('embed', input)).rejects.toMatchObject({
			status: 422,
			code: 'probe_failed',
		});
		expect(calls[0]!.url).toBe('http://api/api/providers/slots/embed');
		expect(calls[0]!.init.method).toBe('PUT');
		expect(JSON.parse(calls[0]!.init.body as string).api_key).toBe('k-1');
	});
});

// ---- LLM providers (spec 2026-10-18 §2) -------------------------------------------------------------------------

describe('LLM providers fixtures match providers.ts', () => {
	const body: LlmProviders = read('llm_providers');

	it('has the root, provider, use and platform-default keys, and never a token', () => {
		expect(keys(body)).toEqual(mapKeys(LLM_SHAPES.ROOT));
		expect(body.providers.length).toBeGreaterThan(0);
		for (const p of body.providers) {
			expect(keys(p), p.name).toEqual(mapKeys(LLM_SHAPES.PROVIDER));
			expect(LLM_KINDS).toContain(p.kind);
		}
		for (const u of body.uses) {
			expect(keys(u)).toEqual(mapKeys(LLM_SHAPES.USE));
			expect(LLM_USES).toContain(u.use);
		}
		expect(keys(body.platform_default)).toEqual(mapKeys(LLM_SHAPES.PLATFORM_DEFAULT));
		expect(JSON.stringify(body)).not.toMatch(/"token"|"secret/);
	});

	it('the test result carries ok, status, latency and the model names', () => {
		const t = read('llm_provider_test');
		expect(keys(t)).toEqual(mapKeys(LLM_SHAPES.TEST));
		expect(Array.isArray(t.models)).toBe(true);
		expect(testLine(t)).toBe('ok · HTTP 200 · 84 ms');
		expect(testLine({ ok: false, status: null, latency_ms: 1500, models: [] })).toBe(
			'failed · no answer · 1500 ms',
		);
	});
});

describe('LLM provider uses', () => {
	const base: LlmProviders = read('llm_providers');
	const openai = base.providers.find((p) => p.kind === 'openai')!;
	const laya = base.providers.find((p) => p.kind === 'laya')!;
	const unbound = (pd: LlmProviders['platform_default']): LlmProviders => ({
		...base,
		uses: [],
		platform_default: pd,
	});

	it('a bound use names its provider and model', () => {
		expect(useSummary(base, 'qualify')).toEqual({ word: 'ok', text: `${openai.name} · local-llm` });
		expect(usesOf(base, openai.id)).toEqual(['qualify']);
		expect(usesOf(base, laya.id)).toEqual([]);
	});

	it('unbound qualify falls back to the platform default, or to keyword rules without one', () => {
		const pd = { base_url: 'http://vllm.example:8001/v1', model: 'local-llm', configured: true };
		expect(useSummary(unbound(pd), 'qualify')).toEqual({
			word: 'idle',
			text: 'Platform default: local-llm @ http://vllm.example:8001/v1',
		});
		expect(useSummary(unbound({ base_url: null, model: null, configured: false }), 'qualify')).toEqual({
			word: 'warn',
			text: 'No LLM — keyword rules',
		});
	});

	it('unbound gates are off, also when the uses list omits the row', () => {
		expect(useSummary(base, 'gates')).toEqual({ word: 'warn', text: 'Laya gates off' });
		expect(bindingOf(unbound(base.platform_default), 'gates')).toEqual({
			use: 'gates',
			provider_id: null,
			model: null,
		});
	});

	it('each use is offered only providers of its kind', () => {
		expect(providersFor(base, 'qualify').map((p) => p.kind)).toEqual(['openai']);
		expect(providersFor(base, 'gates').map((p) => p.id)).toEqual([laya.id]);
		expect(USE_RULES.qualify.kind).toBe('openai');
		expect(USE_RULES.gates.kind).toBe('laya');
		expect(USE_RULES.research.kind).toBe('openai');
		expect(USE_RULES.research_write.kind).toBe('openai');
		expect(providersFor(base, 'research').map((p) => p.id)).toEqual([openai.id]);
		expect(providersFor(base, 'research_write').map((p) => p.id)).toEqual([openai.id]);
	});

	it('lists every use the server binds, each with its label', () => {
		expect(LLM_USES).toEqual(['qualify', 'gates', 'research', 'research_write']);
		expect(USE_RULES.research.label).toBe('Research (plan/read/act)');
		expect(USE_RULES.research_write.label).toBe('Research memo writer');
		expect(base.uses.map((u) => u.use)).toEqual([...LLM_USES]);
	});

	it('unbound research runs on the platform default; unbound research_write borrows research', () => {
		const pd = { base_url: 'http://vllm.example:8001/v1', model: 'local-llm', configured: true };
		expect(useSummary(unbound(pd), 'research')).toEqual({
			word: 'idle',
			text: 'Platform default: local-llm @ http://vllm.example:8001/v1',
		});
		expect(useSummary(unbound({ base_url: null, model: null, configured: false }), 'research')).toEqual({
			word: 'warn',
			text: 'No LLM — research runs fail',
		});
		expect(useSummary(unbound(pd), 'research_write')).toEqual({
			word: 'idle',
			text: 'Same as Research (plan/read/act): Platform default: local-llm @ http://vllm.example:8001/v1',
		});
		const researchBound: LlmProviders = {
			...base,
			uses: [{ use: 'research', provider_id: openai.id, model: 'qwen' }],
		};
		expect(useSummary(researchBound, 'research')).toEqual({ word: 'ok', text: `${openai.name} · qwen` });
		expect(useSummary(researchBound, 'research_write')).toEqual({
			word: 'idle',
			text: `Same as Research (plan/read/act): ${openai.name} · qwen`,
		});
	});

	it('the model picker lists the last test, keeping the bound model; no test = free text', () => {
		const t = { ok: true, status: 200, latency_ms: 5, models: ['a', 'b'] };
		expect(modelChoices(undefined, 'x')).toBeNull();
		expect(modelChoices({ ...t, models: [] }, 'x')).toBeNull();
		expect(modelChoices(t, 'a')).toEqual(['a', 'b']);
		expect(modelChoices(t, 'old')).toEqual(['old', 'a', 'b']);
	});
});

describe('LLM provider form', () => {
	const stored: LlmProvider = {
		id: 'p1',
		name: 'Laya',
		kind: 'laya',
		base_url: 'https://laya.example.eu',
		has_token: true,
		updated_at: '2026-10-18T09:00:00Z',
		updated_by: 'admin',
	};

	it('the token is never pre-filled, and blank on edit keeps the stored one', () => {
		const d = llmDraftFor(stored);
		expect(d.token).toBe('');
		expect(toLlmInput(d)).toEqual({ name: 'Laya', kind: 'laya', base_url: 'https://laya.example.eu' });
		expect(toLlmInput(d)).not.toHaveProperty('token');
	});

	it('a typed token is sent trimmed; Clear token sends an empty string even if one was typed', () => {
		const d = llmDraftFor(stored);
		expect(toLlmInput({ ...d, token: '  t-1 ' }).token).toBe('t-1');
		expect(toLlmInput({ ...d, token: 't-1', clearToken: true }).token).toBe('');
	});

	it('base_url: http(s) only, no userinfo, query or fragment', () => {
		expect(baseUrlProblem('https://laya.example.eu')).toBe('');
		expect(baseUrlProblem('http://93.184.216.34:8001/v1')).toBe('');
		expect(baseUrlProblem('https://[2a00:1450:4001::1]/v1')).toBe('');
		expect(baseUrlProblem('')).toMatch(/Enter/);
		expect(baseUrlProblem('ftp://x.example')).toMatch(/http or https/);
		expect(baseUrlProblem('https://u:p@x.example')).toMatch(/No user or password/);
		expect(baseUrlProblem('https://x.example/v1?k=1')).toMatch(/No query/);
		expect(baseUrlProblem('https://x.example/v1#a')).toMatch(/No fragment/);
		expect(baseUrlProblem('x.example')).toMatch(/Not a URL/);
	});

	it('base_url: a public host only, as the server (netguard) rules', () => {
		for (const u of [
			'http://llm.internal:8001/v1',
			'https://API.INTERNAL./v1',
			'http://carebus-api.some-ns.svc/v1',
			'http://x.svc.cluster.local/v1',
			'http://printer.local:9131/v1',
			'http://nas.lan/v1',
			'http://app.localhost/v1',
			'http://router.home.arpa/v1',
			'http://localhost:8001/v1',
			'http://minio:9000/v1',
		])
			expect(baseUrlProblem(u), u).toMatch(/public host name/);
		for (const u of [
			'http://127.0.0.1:8001/v1',
			'http://2130706433/v1',
			'http://0x7f.1/v1',
			'http://10.0.0.1/v1',
			'http://172.16.0.1/v1',
			'http://192.168.1.119/v1',
			'http://169.254.169.254/latest',
			'http://100.64.0.1/v1',
			'http://100.64.0.7/v1',
			'http://0.0.0.0/v1',
			'http://224.0.0.1/v1',
			'http://240.0.0.1/v1',
			'http://[::1]/v1',
			'http://[::]/v1',
			'http://[fd12:3456::1]/v1',
			'http://[fe80::1]/v1',
			'http://[::ffff:10.0.0.1]/v1',
			'http://[64:ff9b::a9fe:a9fe]/v1',
			'http://[2002:7f00:1::1]/v1',
			'http://[2001::1]/v1',
		])
			expect(baseUrlProblem(u), u).toMatch(/public host:/);
		expect(baseUrlProblem('http://100.63.255.255/v1')).toBe('');
		expect(baseUrlProblem('http://172.32.0.1/v1')).toBe('');
		expect(baseUrlProblem('http://[::ffff:8.8.8.8]/v1')).toBe('');
		expect(baseUrlProblem('https://internal.example.com/v1')).toBe('');
	});

	it('validation names each bad field', () => {
		expect(validateLlmDraft(llmDraftFor(stored))).toEqual({});
		const e = validateLlmDraft({ ...llmDraftFor(null), kind: 'other' as never });
		expect(Object.keys(e).sort()).toEqual(['base_url', 'kind', 'name']);
	});
});

describe('LLM provider form: Test and Use for', () => {
	const base: LlmProviders = read('llm_providers');
	const vllm = base.providers.find((p) => p.kind === 'openai')!;
	const laya = base.providers.find((p) => p.kind === 'laya')!;

	it('Test sends a typed token; blank on edit names the provider so its stored token is used', () => {
		const d = { ...llmDraftFor(vllm), base_url: ' https://llm.example.eu/v1 ' };
		expect(toTestInput({ ...d, token: ' t-1 ' }, vllm.id)).toEqual({
			kind: 'openai',
			base_url: 'https://llm.example.eu/v1',
			token: 't-1',
		});
		expect(toTestInput(d, vllm.id)).toEqual({
			kind: 'openai',
			base_url: 'https://llm.example.eu/v1',
			provider_id: vllm.id,
		});
	});

	it('Test of a new provider without a token, or after Clear token, sends neither token nor provider_id', () => {
		expect(toTestInput({ ...llmDraftFor(null), base_url: 'https://x.example/v1' }, null)).toEqual({
			kind: 'openai',
			base_url: 'https://x.example/v1',
		});
		const cleared = { ...llmDraftFor(vllm), token: 'typed', clearToken: true };
		expect(toTestInput(cleared, vllm.id)).toEqual({ kind: 'openai', base_url: vllm.base_url });
	});

	it('a new URL without the token reads as "enter the token again"; other codes are not explained here', () => {
		expect(draftTestProblem('token_required_for_new_base_url')).toMatch(
			/^Enter the token again for a new URL/,
		);
		expect(draftTestProblem('invalid_body')).toBeNull();
	});

	it('each kind serves its uses', () => {
		expect(usesForKind('openai')).toEqual(['qualify', 'research', 'research_write']);
		expect(usesForKind('laya')).toEqual(['gates']);
	});

	it('Use for starts checked, with its model, only where this provider is bound', () => {
		expect(useForInitial(base, vllm.id)).toEqual({
			qualify: { on: true, model: 'local-llm' },
			gates: { on: false, model: '' },
			research: { on: false, model: '' },
			research_write: { on: false, model: '' },
		});
		expect(useForInitial(base, laya.id).gates).toEqual({ on: false, model: '' });
		expect(useForInitial(base, null).qualify).toEqual({ on: false, model: '' });
		expect(useForInitial(null, vllm.id).qualify.on).toBe(false);
	});

	it('Save binds a checked use, rebinds a changed model, unbinds an unchecked binding, else sends nothing', () => {
		// A new laya provider, checked: bind it to gates.
		expect(useForPlan(base, null, 'gates', { on: true, model: ' jev-latest ' }, 'new-1')).toEqual({
			provider_id: 'new-1',
			model: 'jev-latest',
		});
		// Already bound with the same model: nothing to send.
		expect(useForPlan(base, vllm.id, 'qualify', { on: true, model: 'local-llm' }, vllm.id)).toBeNull();
		// Same provider, other model: rebind.
		expect(useForPlan(base, vllm.id, 'qualify', { on: true, model: 'qwen' }, vllm.id)).toEqual({
			provider_id: vllm.id,
			model: 'qwen',
		});
		// Unchecked while bound here: unbind.
		expect(useForPlan(base, vllm.id, 'qualify', { on: false, model: 'local-llm' }, vllm.id)).toEqual({
			provider_id: null,
			model: null,
		});
		// Unchecked and bound to another provider (or none): leave it.
		expect(useForPlan(base, 'other', 'qualify', { on: false, model: '' }, 'other')).toBeNull();
		expect(useForPlan(base, laya.id, 'gates', { on: false, model: '' }, laya.id)).toBeNull();
		// A blank model binds with none named.
		expect(useForPlan(base, laya.id, 'gates', { on: true, model: '' }, laya.id)).toEqual({
			provider_id: laya.id,
			model: null,
		});
	});

	it('the form model picker lists the Test models; without them Laya suggests its known models', () => {
		const t = { ok: true, status: 200, latency_ms: 5, models: ['jev-system1'] };
		expect(formModelChoices('laya', t, 'jev-latest')).toEqual({
			choices: ['jev-latest', 'jev-system1'],
			suggestions: LAYA_MODEL_SUGGESTIONS,
		});
		expect(formModelChoices('laya', undefined, null)).toEqual({
			choices: null,
			suggestions: ['jev-latest', 'laya-multilingual', 'laya-english'],
		});
		expect(formModelChoices('openai', undefined, null)).toEqual({ choices: null, suggestions: [] });
	});

	it('an unbound use says loudly what runs instead; a bound one says nothing', () => {
		expect(unboundNotice(base, 'qualify')).toBeNull();
		expect(unboundNotice(base, 'gates')).toBe('gates: not bound — Laya gates are off, no gate question runs');
		const none = { ...base, uses: [] };
		expect(unboundNotice(none, 'qualify')).toBe(
			'qualify: not bound — chat uses the platform default (local-llm @ http://vllm.example:8001/v1)',
		);
		expect(
			unboundNotice(
				{ ...none, platform_default: { base_url: null, model: null, configured: false } },
				'qualify',
			),
		).toBe('qualify: not bound — requests are qualified by keyword rules, no LLM');
		expect(unboundNotice(none, 'research')).toBe(
			'research: not bound — research runs use the platform default (local-llm @ http://vllm.example:8001/v1)',
		);
		expect(
			unboundNotice(
				{ ...none, platform_default: { base_url: null, model: null, configured: false } },
				'research',
			),
		).toBe('research: not bound — no platform default is configured, research runs cannot start');
		expect(unboundNotice(none, 'research_write')).toBe(
			'research_write: not bound — the research binding writes the memo (Platform default: local-llm @ http://vllm.example:8001/v1)',
		);
	});
});

describe('LLM providers client', () => {
	it('sends each call to the spec route with the right method and body', async () => {
		const calls: { url: string; method: string; body: unknown }[] = [];
		const fetch = (async (url: string, init: RequestInit) => {
			calls.push({ url, method: init.method!, body: init.body ? JSON.parse(init.body as string) : null });
			return init.method === 'DELETE' ? new Response(null, { status: 204 }) : new Response('{}');
		}) as unknown as typeof globalThis.fetch;
		const c = createClient({ baseUrl: 'http://api', token: async () => 't', job: () => 'j', fetch });
		await c.llmProviders();
		await c.createLlmProvider({ name: 'L', kind: 'laya', base_url: 'https://l.example', token: 'x' });
		await c.updateLlmProvider('p 1', { name: 'L', kind: 'laya', base_url: 'https://l.example', token: '' });
		await c.deleteLlmProvider('p1');
		await c.testLlmProvider('p1');
		await c.testLlmProviderDraft({ kind: 'laya', base_url: 'https://l.example', provider_id: 'p1' });
		await c.bindLlmUse('gates', { provider_id: 'p1', model: 'jev-system1' });
		await c.providerSlots();
		await c.removeProviderSlot('ocr');
		expect(calls.map((x) => `${x.method} ${x.url}`)).toEqual([
			'GET http://api/api/providers',
			'POST http://api/api/providers',
			'PUT http://api/api/providers/p%201',
			'DELETE http://api/api/providers/p1',
			'POST http://api/api/providers/p1/test',
			'POST http://api/api/providers/test',
			'PUT http://api/api/providers/uses/gates',
			'GET http://api/api/providers/slots',
			'DELETE http://api/api/providers/slots/ocr',
		]);
		expect(calls[2]!.body).toEqual({ name: 'L', kind: 'laya', base_url: 'https://l.example', token: '' });
		expect(calls[5]!.body).toEqual({ kind: 'laya', base_url: 'https://l.example', provider_id: 'p1' });
		expect(calls[6]!.body).toEqual({ provider_id: 'p1', model: 'jev-system1' });
	});

	it('a provider still bound to a use comes back as 409 provider_in_use', async () => {
		const fetch = (async () =>
			new Response(JSON.stringify({ error: { code: 'provider_in_use', message: 'bound to gates' } }), {
				status: 409,
			})) as unknown as typeof globalThis.fetch;
		const c = createClient({ baseUrl: 'http://api', token: async () => 't', fetch });
		await expect(c.deleteLlmProvider('p1')).rejects.toMatchObject({ status: 409, code: 'provider_in_use' });
	});
});
