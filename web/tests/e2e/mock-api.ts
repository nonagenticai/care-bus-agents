// Serves the generated fixtures in src/lib/api/fixtures/ (plan 05's underscore names) as the default bodies, so a
// mock cannot drift from types.ts. Specs that assert VALUES pass a hand-written overlay from tests/e2e/fixtures/
// through `override`, because plan 05 regenerates the generated files with different values (ruling F7).
import type { Page, Route } from '@playwright/test';
import { readFileSync } from 'node:fs';

const API = 'http://api.e2e.test';
const read = (dir: string, name: string) => JSON.parse(readFileSync(`${dir}/${name}.json`, 'utf8'));
const fx = (name: string) => read('src/lib/api/fixtures', name);
/** Hand-written, value-carrying response bodies the specs assert on. */
export const overlay = (name: string) => read('tests/e2e/fixtures', name);

const ok = (route: Route, body: unknown, status = 200) =>
	route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });

const FIXED: Record<string, string> = {
	'/api/overview': 'overview',
	'/api/analytics': 'analytics',
	'/api/agents': 'agents',
	'/api/streams': 'streams',
	'/api/dead-letters': 'dead_letters',
	'/api/cases': 'cases',
	'/api/connectors': 'connectors',
	'/api/context-spaces': 'context_spaces',
	'/api/ontology/entities': 'ontology_entities',
	'/api/ontology/relations': 'ontology_relations',
	'/api/procedures': 'procedures',
	'/api/knowledge/stores': 'knowledge_stores',
	'/api/tools': 'tools',
	'/api/audit': 'audit',
	'/api/demo/scenarios': 'demo_scenarios',
	'/api/demo/faults': 'demo_faults',
	'/api/sources': 'sources',
	'/api/ingest/jobs': 'ingest_jobs',
	'/api/providers/slots': 'providers/providers',
};

/** POST /api/procedures with routes::procedures::create's version and steps-mirror-the-plan checks and codes. */
function mockCreateProcedure(route: Route) {
	const r = route.request().postDataJSON();
	const bad = (status: number, code: string, message: string) =>
		ok(route, { error: { code, message } }, status);
	if (!/^\d{1,9}\.\d{1,9}\.\d{1,9}(-[A-Za-z0-9.]+)?$/.test(r.version ?? ''))
		return bad(
			400,
			'invalid_version',
			'version must look like 1.4.0 or 1.4.0-beta (numbers up to 9 digits).',
		);
	const phases: { steps: { worker: string }[] }[] = Array.isArray(r.plan?.phases) ? r.plan.phases : [];
	const want = phases.flatMap((ph, i) => (ph.steps ?? []).map((st) => `${i}:${st.worker}`));
	const got = [...(r.steps ?? [])]
		.sort((a: { step_number: number }, b: { step_number: number }) => a.step_number - b.step_number)
		.map((st: { phase: number; worker: string }) => `${st.phase}:${st.worker}`);
	if (!want.length) return bad(400, 'invalid_plan', 'plan is not runnable: a plan needs at least one step');
	if (want.join() !== got.join())
		return bad(
			400,
			'steps_do_not_match_plan',
			`steps must describe the plan one-to-one: the plan has ${want.length} step(s); ordered by step_number, each step needs the phase and worker of the matching plan step.`,
		);
	return ok(
		route,
		{ ...fx('procedure_version'), name: r.name, version: r.version, version_status: 'DRAFT', reviewers: [] },
		201,
	);
}

/** POST /api/ontology/relations with routes::ontology::create_relation's checks and error codes. */
function mockCreateRelation(
	route: Route,
	relations: { subject: string; predicate: string; object: string; cardinality: string }[],
) {
	const r = route.request().postDataJSON();
	const known = new Set(fx('ontology_entities').items.map((x: { name: string }) => x.name));
	const bad = (status: number, code: string, message: string) =>
		ok(route, { error: { code, message } }, status);
	if (!/^[a-z][A-Za-z0-9]{0,63}$/.test(r.predicate ?? ''))
		return bad(400, 'invalid_body', 'predicate must be a lowerCamel verb such as places or paidBy.');
	if (!['1:1', '1:n', 'n:1', 'n:m'].includes(r.cardinality))
		return bad(400, 'invalid_body', 'cardinality must be one of 1:1, 1:n, n:1, n:m.');
	for (const role of ['subject', 'object'] as const)
		if (!known.has(r[role]))
			return bad(400, 'unknown_entity', `${role} ${r[role]} is not an existing entity.`);
	if (relations.some((x) => x.subject === r.subject && x.predicate === r.predicate && x.object === r.object))
		return bad(409, 'relation_exists', `${r.subject} ${r.predicate} ${r.object} already exists.`);
	const rel = { subject: r.subject, predicate: r.predicate, object: r.object, cardinality: r.cardinality };
	relations.push(rel);
	return ok(route, rel, 201);
}

/**
 * One request the page made. `path` is the route with the job prefix taken off (`/api/jobs/{job}/cases` is
 * recorded as `/api/cases`, with `job` = its key), so specs name routes as before; `job` is null for tenant
 * routes (`/api/knowledge/…`, `/api/providers`, `/api/me`, …). `tenant` is the `x-active-tenant-id` sent, if any.
 */
export type Sent = {
	method: string;
	path: string;
	job: string | null;
	tenant: string | null;
	search: string;
	body: string | null;
};

export const E2E_TENANT = '0195f48b-d000-7000-8000-000000000099';
export const E2E_JOB = 'customer_care_agent';
/** Routes carebus-api nests under `/api/jobs/{job}` (crates/carebus-api/src/routes/mod.rs `job_routes`). */
const JOB_ROUTES = new Set([
	'agents',
	'analytics',
	'cases',
	'connectors',
	'context-spaces',
	'conversations',
	'dead-letters',
	'demo',
	'events',
	'knowledge-stores',
	'ontology',
	'overview',
	'procedures',
	'refinements',
	'release-conflicts',
	'releases',
	'settings',
	'sources',
	'streams',
	'tools',
]);

/**
 * The URL the app requests for `path` (e.g. `/api/cases?*`), for specs' own `page.route` overrides: job routes
 * get the `/api/jobs/{E2E_JOB}` prefix like client.ts `J(...)`, tenant routes stay at `/api/...`.
 */
export function apiUrl(path: string): string {
	const [, api, first] = path.split(/[/?]/);
	if (api !== 'api') throw new Error(`apiUrl expects an /api/... path, got ${path}`);
	return JOB_ROUTES.has(first ?? '') ? `${API}/api/jobs/${E2E_JOB}${path.slice(4)}` : `${API}${path}`;
}

/** The actor the fake e2e auth signs in as (src/lib/auth/keycloak.ts). */
const E2E_ACTOR = 'e2e';

type VersionState = { version_status: string; reviewers: string[] };

/**
 * Procedure versions are stateful like routes::procedures: `approve` adds the caller to a CANARY version's reviewers
 * (409 not_in_review / already_approved), and `status` to PUBLISHED needs two distinct reviewers (409
 * needs_two_reviewers, nothing written) and archives the name's current PUBLISHED version.
 */
function procedureStore() {
	const items: ({ name: string; version: string } & VersionState)[] = fx('procedures').items;
	const state = new Map<string, VersionState>(
		items.map((i) => [
			`${i.name}@${i.version}`,
			{ version_status: i.version_status, reviewers: [...i.reviewers] },
		]),
	);
	const get = (name: string, version: string): VersionState => {
		const k = `${name}@${version}`;
		if (!state.has(k)) state.set(k, { version_status: 'DRAFT', reviewers: [] });
		return state.get(k)!;
	};
	const detail = (name: string, version: string) => ({
		...fx('procedure_version'),
		name,
		version,
		...get(name, version),
	});
	const conflict = (route: Route, code: string, message: string) =>
		ok(route, { error: { code, message } }, 409);
	return {
		list: () => ({ ...fx('procedures'), items: items.map((i) => ({ ...i, ...get(i.name, i.version) })) }),
		detail: (route: Route, name: string, version: string) => ok(route, detail(name, version)),
		approve(route: Route, name: string, version: string) {
			const v = get(name, version);
			if (v.version_status !== 'CANARY')
				return conflict(
					route,
					'not_in_review',
					`only a CANARY version can be approved; ${name}@${version} is ${v.version_status}`,
				);
			if (v.reviewers.includes(E2E_ACTOR))
				return conflict(route, 'already_approved', `${E2E_ACTOR} has already approved ${name}@${version}`);
			v.reviewers = [...v.reviewers, E2E_ACTOR].sort();
			return ok(route, detail(name, version));
		},
		status(route: Route, name: string, version: string) {
			const to: string = route.request().postDataJSON().to;
			const v = get(name, version);
			if (to === 'PUBLISHED') {
				if (new Set(v.reviewers).size < 2)
					return conflict(
						route,
						'needs_two_reviewers',
						`${v.reviewers.length} reviewer(s) so far; publishing needs two distinct reviewers`,
					);
				for (const [k, other] of state)
					if (k.startsWith(`${name}@`) && other.version_status === 'PUBLISHED')
						other.version_status = 'ARCHIVED';
			}
			v.version_status = to;
			return ok(route, detail(name, version));
		},
	};
}

type LlmRow = {
	id: string;
	name: string;
	kind: string;
	base_url: string;
	has_token: boolean;
	updated_at: string;
	updated_by: string | null;
};

/**
 * `/api/providers` LLM providers, stateful like routes::providers: writes show up on the next GET, a token is never
 * echoed (only `has_token`), DELETE answers 409 `provider_in_use` while a use is bound, and a use takes only its
 * kind (qualify → openai, gates → laya).
 */
function llmProviderStore() {
	const body = fx('providers/llm_providers');
	const providers: LlmRow[] = body.providers;
	const uses: { use: string; provider_id: string | null; model: string | null }[] = body.uses;
	let n = 0;
	const bad = (route: Route, status: number, code: string, message: string) =>
		ok(route, { error: { code, message } }, status);
	const apply = (row: LlmRow, b: { name: string; kind: string; base_url: string; token?: string }) => {
		row.name = b.name;
		row.kind = b.kind;
		row.base_url = b.base_url;
		if (b.token !== undefined) row.has_token = b.token !== '';
		row.updated_at = '2026-10-18T12:00:00Z';
		row.updated_by = E2E_ACTOR;
		return row;
	};
	return {
		handle(route: Route, method: string, p: string) {
			const [, , id, sub] = p.split('/').filter(Boolean); // ['api', 'providers', id, sub]
			if (!id && method === 'GET') return ok(route, { ...body, providers, uses });
			if (!id && method === 'POST') {
				const b = route.request().postDataJSON();
				const row = apply(
					{
						id: `0195f48b-d001-7000-8000-00000000f${String(++n).padStart(3, '0')}`,
						has_token: false,
					} as LlmRow,
					b,
				);
				providers.push(row);
				return ok(route, row, 201);
			}
			// POST /api/providers/test: unsaved form values. `provider_id` without a token lends the stored token, for
			// the stored base_url only (422 token_required_for_new_base_url otherwise).
			if (id === 'test' && !sub && method === 'POST') {
				const b = route.request().postDataJSON();
				if (!b.token && b.provider_id) {
					const prov = providers.find((x) => x.id === b.provider_id);
					if (!prov) return bad(route, 404, 'not_found', 'no such provider');
					if (prov.has_token && prov.base_url !== b.base_url)
						return bad(
							route,
							422,
							'token_required_for_new_base_url',
							'Changing base_url needs the token again; the stored token is never sent to a new host.',
						);
				}
				return ok(route, {
					...fx('providers/llm_provider_test'),
					...(b.kind === 'openai' ? { models: ['local-llm', 'qwen3-27b'] } : {}),
				});
			}
			if (id === 'uses' && sub && method === 'PUT') {
				const b = route.request().postDataJSON();
				const want = sub === 'gates' ? 'laya' : 'openai';
				const prov = providers.find((x) => x.id === b.provider_id);
				if (b.provider_id && !prov) return bad(route, 404, 'not_found', 'no such provider');
				if (prov && prov.kind !== want)
					return bad(route, 400, 'invalid_body', `${sub} needs a ${want} provider`);
				const row = { use: sub, provider_id: b.provider_id ?? null, model: b.model ?? null };
				const i = uses.findIndex((u) => u.use === sub);
				if (i >= 0) uses[i] = row;
				else uses.push(row);
				return ok(route, row);
			}
			const row = providers.find((x) => x.id === decodeURIComponent(id ?? ''));
			if (!row) return null;
			if (sub === 'test' && method === 'POST') return ok(route, fx('providers/llm_provider_test'));
			if (!sub && method === 'PUT') return ok(route, apply(row, route.request().postDataJSON()));
			if (!sub && method === 'DELETE') {
				const bound = uses.filter((u) => u.provider_id === row.id).map((u) => u.use);
				if (bound.length)
					return bad(route, 409, 'provider_in_use', `provider is bound to ${bound.join(', ')}`);
				providers.splice(providers.indexOf(row), 1);
				return route.fulfill({ status: 204 });
			}
			return null;
		},
	};
}

/**
 * Mocks carebus-api. `down` lists path prefixes that answer 503 so pages must show "Unavailable".
 * `override` replaces the body served for an exact path (e.g. an overview with no resolved cases); both name
 * job routes without the job prefix. `role` is the tenant role `GET /api/me` reports (default `tenant_admin`);
 * `me` replaces that body entirely (super_admin, unprovisioned). A job route requested without the
 * `/api/jobs/{job}` prefix answers 404, like carebus-api.
 */
export async function mockApi(
	page: Page,
	opts: {
		down?: string[];
		override?: Record<string, unknown>;
		role?: 'tenant_admin' | 'tenant_user';
		me?: Record<string, unknown>;
		/** `GET /api/jobs` items (default: the bootstrap job only). */
		jobs?: { key: string; label: string; on_bus?: boolean }[];
	} = {},
): Promise<Sent[]> {
	const me = opts.me ?? {
		sub: E2E_ACTOR,
		actor: E2E_ACTOR,
		super_admin: false,
		release_writer: false,
		tenant: { id: E2E_TENANT, name: 'Bootstrap tenant', role: opts.role ?? 'tenant_admin', status: 'active' },
		active_tenant: E2E_TENANT,
	};
	const sent: Sent[] = [];
	const sse: string[] = [];
	// Ontology relations are stateful so a write shows up on the next GET, like routes::ontology.
	const ontologyRelations: { subject: string; predicate: string; object: string; cardinality: string }[] =
		fx('ontology_relations').items;
	const procedures = procedureStore();
	const llm = llmProviderStore();
	await page.route(`${API}/**`, async (route) => {
		const url = new URL(route.request().url());
		const method = route.request().method();
		const raw = url.pathname.split('/').filter(Boolean); // ['api', 'jobs', 'customer_care_agent', 'streams', ...]
		const scoped = raw[0] === 'api' && raw[1] === 'jobs' && raw.length > 3;
		const job = scoped ? decodeURIComponent(raw[2]!) : null;
		const p = scoped ? `/api/${raw.slice(3).join('/')}` : url.pathname;
		sent.push({
			method,
			path: p,
			job,
			tenant: route.request().headers()['x-active-tenant-id'] ?? null,
			search: url.search,
			body: route.request().postData(),
		});
		const parts = p.split('/').filter(Boolean); // ['api', 'streams', ...]
		if (!scoped && JOB_ROUTES.has(parts[1] ?? ''))
			return ok(route, { error: { code: 'not_found', message: `route not found: ${url.pathname}` } }, 404);
		if (p === '/api/me' && method === 'GET') return ok(route, me);
		if (p === '/api/jobs' && method === 'GET')
			return ok(route, {
				items: (opts.jobs ?? [{ key: E2E_JOB, label: 'Customer care' }]).map((j) => ({
					settings: {},
					on_bus: true,
					...j,
				})),
				next: null,
			});
		if (opts.down?.some((d) => p.startsWith(d)))
			return ok(
				route,
				{ error: { code: 'dependency_unavailable', message: 'case_history is unreachable' } },
				503,
			);
		if (opts.override && p in opts.override) return ok(route, opts.override[p]);

		if (p === '/api/events') {
			// Hold until a message has been sent, then stream what it produced and drain the queue so a
			// reconnect does not replay the same frames (F12). An idle poll ends with an empty body.
			for (let i = 0; i < 50 && sse.length === 0; i++) await new Promise((r) => setTimeout(r, 100));
			return route.fulfill({
				status: 200,
				headers: { 'content-type': 'text/event-stream' },
				body: sse.splice(0).join(''),
			});
		}
		if (p === '/api/conversations' && method === 'POST')
			return ok(
				route,
				{
					conversation_id: 'conv-1',
					customer_id: 'C-10482',
					customer_name: 'Anna de Vries',
					customer_tier: 'Gold',
				},
				201,
			);
		if (p === '/api/conversations/conv-1/messages') {
			// ChatEvent omits case_id when it has none (carebus chat.rs skip_serializing_if): never send null.
			const chat = (role: 'customer' | 'agent', text: string, case_id: string | undefined, at_ms: number) =>
				`event: chat\nid: chat-${at_ms}\ndata: ${JSON.stringify({ conversation_id: 'conv-1', role, text, ...(case_id ? { case_id } : {}), at_ms })}\n\n`;
			const body = opts.override?.['/api/cases/CASE-1'] ?? fx('case');
			const trace = (
				body as {
					trace: {
						stream: string;
						entry_id: string;
						consumer_group?: string | null;
						acked?: boolean | null;
					}[];
				}
			).trace;
			// Like routes/events.rs: a bus frame carries the publish-time ack state (false), and the recipient's
			// XACK follows as `event: ack` for each entry the trace reports acked.
			sse.push(
				chat('customer', 'Where is my airfryer?', undefined, 1),
				...trace.map(
					(e) =>
						`event: bus\nid: ${e.entry_id}\ndata: ${JSON.stringify({ ...e, acked: e.consumer_group ? false : null })}\n\n`,
				),
				...trace
					.filter((e) => e.acked === true)
					.map(
						(e) =>
							`event: ack\ndata: ${JSON.stringify({ stream: e.stream, entry_id: e.entry_id, consumer_group: e.consumer_group, correlation_id: 'CASE-1' })}\n\n`,
					),
				': ping\n\n',
				chat('agent', 'Your Philips Airfryer XXL (ORD-58790) is on its way with PostNL.', 'CASE-1', 2),
			);
			return ok(route, fx('conversation_message'));
		}
		if (p === '/api/conversations/conv-1') return ok(route, fx('conversation'));

		if (p === '/api/ontology/relations' && method === 'GET')
			return ok(route, { items: ontologyRelations, next: null });
		if (p === '/api/ontology/relations' && method === 'POST')
			return mockCreateRelation(route, ontologyRelations);
		if (p === '/api/ontology/entities' && method === 'POST') {
			const body = route.request().postDataJSON();
			return ok(route, { ...body, hidden: false }, 201);
		}
		if (p === '/api/procedures' && method === 'POST') return mockCreateProcedure(route);
		if (p === '/api/procedures' && method === 'GET') return ok(route, procedures.list());
		if (
			p === '/api/providers' ||
			(p.startsWith('/api/providers/') && !p.startsWith('/api/providers/slots'))
		) {
			const handled = llm.handle(route, method, p);
			if (handled) return handled;
		}
		if (method === 'GET' && FIXED[p]) return ok(route, fx(FIXED[p]!));

		// Item routes (GET) and writes (POST/PUT/DELETE) the pages call.
		const [, a, b, c, d, e] = parts;
		if (a === 'agents' && b) return ok(route, fx('agent'));
		if (a === 'cases' && b) return ok(route, fx('case'));
		if (a === 'streams' && b && c === 'entries') return ok(route, fx('stream_entries'));
		if (a === 'streams' && b && c === 'groups' && e === 'claim') return ok(route, { claimed: 1 });
		if (a === 'streams' && b)
			return ok(
				route,
				fx('streams').items.find((x: { key: string }) => encodeURIComponent(x.key) === b) ??
					fx('streams').items[0],
			);
		if (a === 'dead-letters' && b && c === 'replay')
			return ok(route, { stream: 'bus:tool:delivery', entry_id: b });
		if (a === 'dead-letters' && b && method === 'DELETE') return ok(route, { deleted: b });
		if (a === 'connectors' && b && c === 'test') return ok(route, fx('connector_test'));
		if (a === 'connectors') return ok(route, fx('connector'));
		if (a === 'ontology' && b === 'entities' && c) return ok(route, { name: decodeURIComponent(c) });
		if (a === 'ontology' && b === 'relations' && c && d && e && method === 'DELETE') {
			const [s, pr, o] = [c, d, e].map(decodeURIComponent);
			const i = ontologyRelations.findIndex((r) => r.subject === s && r.predicate === pr && r.object === o);
			if (i < 0)
				return ok(
					route,
					{ error: { code: 'not_found', message: `relation ${s}/${pr}/${o} not found` } },
					404,
				);
			ontologyRelations.splice(i, 1);
			return route.fulfill({ status: 204 });
		}
		if (a === 'procedures' && b && c === 'versions' && d) {
			const [name, version] = [b, d].map(decodeURIComponent) as [string, string];
			if (e === 'status' && method === 'POST') return procedures.status(route, name, version);
			if (e === 'approve' && method === 'POST') return procedures.approve(route, name, version);
			if (!e) return procedures.detail(route, name, version);
		}
		if (a === 'procedures' && b) return ok(route, fx('procedure_version'));
		if (a === 'knowledge' && b === 'stores' && d === 'documents') return ok(route, fx('knowledge_documents'));
		if (a === 'knowledge' && b === 'query') return ok(route, fx('knowledge_query'));
		if (a === 'demo' && b === 'records' && c) return ok(route, d ? fx('demo_record') : fx('demo_records'));
		if (a === 'demo' && b === 'reset') return ok(route, { reset: true });
		if (a === 'demo' && b === 'scenarios' && c)
			return ok(route, { applied: decodeURIComponent(c), changes: [] });
		if (a === 'demo' && b === 'faults') return ok(route, { ok: true });
		if (a === 'providers' && b === 'slots' && c && (method === 'PUT' || method === 'DELETE'))
			return ok(route, fx('providers/providers'));
		if (a === 'sources' && b === 'preview-columns') return ok(route, fx('source_preview'));
		if (a === 'sources' && b === 'test' && !c) return ok(route, fx('source_test'));
		if (a === 'sources' && b && c === 'test') return ok(route, fx('source_test'));
		if (a === 'sources' && b && c === 'resync') return ok(route, fx('source_run'), 202);
		if (a === 'sources' && b && c === 'runs' && d) return ok(route, fx('source_runs').items[0]);
		if (a === 'sources' && b && c === 'runs') return ok(route, fx('source_runs'));
		if (a === 'sources' && b && method === 'DELETE') return ok(route, fx('source_delete'));
		if (a === 'sources') return ok(route, fx('source'), method === 'POST' ? 201 : 200);
		if (a === 'ingest' && b === 'jobs' && c) return ok(route, fx('ingest_job'));
		if (a === 'knowledge' && b === 'stores' && !c && method === 'POST')
			return ok(route, fx('knowledge_store'), 201);
		if (a === 'knowledge' && (b === 'stores' || b === 'documents') && c && !d && method === 'DELETE')
			return route.fulfill({ status: 204 });
		return ok(route, { error: { code: 'not_mocked', message: `not mocked: ${p}` } }, 404);
	});
	return sent;
}
