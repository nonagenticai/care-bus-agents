import type * as T from './types';
import type {
	LlmProvider,
	LlmProviderInput,
	LlmProviderTest,
	LlmProviderTestInput,
	LlmProviders,
	LlmSettings,
	LlmUse,
	LlmUseBinding,
	LlmUseInput,
	ProviderInput,
	ProviderSlots,
} from './providers';
import type {
	ApproveResult,
	InviteInput,
	InviteResult,
	RetryResult,
	Signup,
	SignupListStatus,
	SignupPage,
} from './signups';
import type {
	ResearchRun,
	ResearchRunDetail,
	ResearchRunInput,
	ResearchRunSummary,
	RunStatus,
} from './research';
import type { AnswerBody, AnswerResult, CancelResult, TaskList, TaskListState, TaskView } from './tasks';
import type { LegalMatter, LegalMatterDetail, LegalMatterInput, LegalMatterPatch, MatterRole } from './legal';

export class ApiError extends Error {
	constructor(
		readonly status: number,
		readonly code: string,
		message: string,
		/** The whole `error` object: some refusals carry more than code and message (e.g. `run_id`). */
		readonly details: Record<string, unknown> = {},
	) {
		super(message);
		this.name = 'ApiError';
	}
}

type Token = (opts?: { force?: boolean }) => Promise<string | null>;
/**
 * `job` names the job the job-scoped routes act in (`/api/jobs/{job}/…`); `activeTenant` is sent as
 * `X-Active-Tenant-Id` on every request when non-null (a super_admin's chosen tenant). Both are read per request.
 */
type Opts = {
	baseUrl: string;
	token: Token;
	job?: () => string;
	activeTenant?: () => string | null;
	fetch?: typeof fetch;
};
type Q = Record<string, string | number | boolean | null | undefined>;

/**
 * Exact port of carebus-api `secrets::looks_like_secret` (plan 05 Task 8): the server is the
 * authority, this only fails fast in the UI. Keep the two in step.
 */
export function looksLikeSecret(s: string): boolean {
	const t = s.trim();
	if (t.startsWith('sk-') || t.startsWith('eyJ') || t.includes('-----BEGIN')) return true;
	if (t.length < 40 || /[\s/:.]/u.test(t)) return false;
	const classes = [/[a-z]/.test(t), /[A-Z]/.test(t), /[0-9]/.test(t)];
	return classes.filter(Boolean).length >= 3;
}

function qs(q?: Q): string {
	if (!q) return '';
	const p = new URLSearchParams();
	for (const [k, v] of Object.entries(q)) if (v !== undefined && v !== null && v !== '') p.set(k, String(v));
	const s = p.toString();
	return s ? `?${s}` : '';
}
const seg = encodeURIComponent;
/** `?layer=` for base or tenant; the effective layer is the default and sends nothing. */
const layerQ = (layer?: 'effective' | 'base' | 'tenant') =>
	qs({ layer: layer === 'effective' ? undefined : layer });

/** The bootstrap job (plan 10 Global Constraints); used until `GET /api/jobs` names the tenant's jobs. */
export const DEFAULT_JOB = 'customer_care_agent';
export const ACTIVE_TENANT_HEADER = 'x-active-tenant-id';

export function createClient(o: Opts) {
	const f = o.fetch ?? fetch;
	const job = o.job ?? (() => DEFAULT_JOB);
	const activeTenant = o.activeTenant ?? (() => null);
	/** A job-scoped route: carebus-api nests these under `/api/jobs/{job}`. */
	const J = (path: string) => `/api/jobs/${seg(job())}${path}`;
	/** `X-Active-Tenant-Id` when a tenant is chosen, else nothing. */
	function scopeHeaders(): Record<string, string> {
		const t = activeTenant();
		return t ? { [ACTIVE_TENANT_HEADER]: t } : {};
	}
	async function send(method: string, path: string, body: unknown, force: boolean): Promise<Response> {
		const token = await (force ? o.token({ force: true }) : o.token());
		const form = typeof FormData !== 'undefined' && body instanceof FormData;
		// A Blob goes out as-is under its own type (a CSV import); FormData sets its own boundary.
		const blob = typeof Blob !== 'undefined' && body instanceof Blob;
		const headers: Record<string, string> = { Accept: 'application/json', ...scopeHeaders() };
		if (token) headers.Authorization = `Bearer ${token}`;
		if (blob) headers['Content-Type'] = (body as Blob).type || 'application/octet-stream';
		else if (body !== undefined && !form) headers['Content-Type'] = 'application/json';
		try {
			return await f(`${o.baseUrl}${path}`, {
				method,
				headers,
				body:
					body === undefined ? undefined : form || blob ? (body as FormData | Blob) : JSON.stringify(body),
			});
		} catch (e) {
			throw new ApiError(0, 'unavailable', `carebus-api is unreachable (${(e as Error).message})`);
		}
	}
	async function req<R>(method: string, path: string, body?: unknown): Promise<R> {
		let res = await send(method, path, body, false);
		// D3: the server rejected a token keycloak-js still considers valid. Refresh it for real, retry once.
		if (res.status === 401) res = await send(method, path, body, true);
		if (res.status === 204) return undefined as R;
		const text = await res.text().catch(() => '');
		let data: unknown = null;
		let parsed = text === '';
		if (text !== '') {
			try {
				data = JSON.parse(text);
				parsed = true;
			} catch {
				/* not JSON: handled below */
			}
		}
		if (res.ok && !parsed) {
			throw new ApiError(res.status, 'bad_response', 'carebus-api returned a non-JSON response');
		}
		if (!res.ok) {
			const err = (data as { error?: { code?: string; message?: string } & Record<string, unknown> } | null)
				?.error;
			throw new ApiError(
				res.status,
				err?.code ?? `http_${res.status}`,
				err?.message ?? res.statusText,
				err && typeof err === 'object' ? err : {},
			);
		}
		return (text === '' ? undefined : data) as R;
	}
	function guardSecretRef(ref: string | null) {
		if (ref !== null && looksLikeSecret(ref)) {
			throw new ApiError(
				400,
				'secret_value',
				'secret_ref must name a secret (e.g. carebus-llm/anthropic-api-key), not contain one',
			);
		}
	}

	/** Fail fast on a pasted key in source config (stored in clear). The server rejects it too; `credentials` is exempt. */
	function guardConfig(config: unknown) {
		const walk = (v: unknown): boolean =>
			typeof v === 'string'
				? looksLikeSecret(v)
				: Array.isArray(v)
					? v.some(walk)
					: v !== null && typeof v === 'object'
						? Object.values(v).some(walk)
						: false;
		if (walk(config)) {
			throw new ApiError(
				400,
				'secret_value',
				'This looks like a key. Put it in the credentials field; source settings are stored in clear.',
			);
		}
	}
	function filesForm(files: File[], field = 'file'): FormData {
		const fd = new FormData();
		for (const f of files) fd.append(field, f, f.name);
		return fd;
	}

	return {
		/** The caller as care-bus knows it: roles, home tenant, active tenant. Any verified caller may ask. */
		me: () => req<T.Me>('GET', '/api/me'),
		/** The active tenant's jobs. */
		jobs: () => req<T.Paged<T.JobSummary>>('GET', '/api/jobs'),
		/** Every tenant (super_admin only): the tenant picker. */
		tenants: () => req<T.Paged<T.TenantSummary>>('GET', '/api/tenants'),
		tenantUsers: (tenant: string) => req<T.Paged<T.TenantUser>>('GET', `/api/tenants/${seg(tenant)}/users`),
		// routes::tenancy::put_user: 201/200; 409 user_in_other_tenant / last_tenant_admin, 403 other tenant.
		saveTenantUser: (tenant: string, sub: string, input: T.TenantUserInput) =>
			req<unknown>('PUT', `/api/tenants/${seg(tenant)}/users/${seg(sub)}`, input),
		// routes::tenancy::delete_user: 204; 404 absent, 409 last_tenant_admin.
		removeTenantUser: (tenant: string, sub: string) =>
			req<void>('DELETE', `/api/tenants/${seg(tenant)}/users/${seg(sub)}`),
		/** The console SSE stream for one conversation (opened by `EventFeed`, not by `req`). */
		eventsUrl: (conversationId: string) =>
			`${o.baseUrl}${J(`/events${qs({ conversation_id: conversationId })}`)}`,
		/** Headers `EventFeed` adds so the SSE request acts in the same tenant as every other call. */
		scopeHeaders,
		overview: () => req<T.Overview>('GET', J('/overview')),
		analytics: (days: 7 | 28) => req<T.Analytics>('GET', J(`/analytics${qs({ days })}`)),
		agents: () => req<T.Paged<T.AgentView>>('GET', J('/agents')),
		agent: (id: string) => req<T.AgentView>('GET', J(`/agents/${seg(id)}`)),
		streams: () => req<T.Paged<T.StreamInfo>>('GET', J('/streams')),
		stream: (key: string) => req<T.StreamInfo>('GET', J(`/streams/${seg(key)}`)),
		streamEntries: (key: string, count = 20) =>
			req<T.Paged<T.Entry>>('GET', J(`/streams/${seg(key)}/entries${qs({ count })}`)),
		claim: (key: string, group: string) =>
			req<{ claimed: number }>('POST', J(`/streams/${seg(key)}/groups/${seg(group)}/claim`)),
		deadLetters: () => req<T.Paged<T.Entry>>('GET', J('/dead-letters')),
		replayDeadLetter: (entryId: string) =>
			req<{ stream: string; entry_id: string }>('POST', J(`/dead-letters/${seg(entryId)}/replay`)),
		discardDeadLetter: (entryId: string) =>
			req<{ deleted: string }>('DELETE', J(`/dead-letters/${seg(entryId)}`)),
		cases: (q: { outcome?: T.CaseFilter; area?: string; limit?: number; before?: string }) =>
			req<T.Paged<T.CaseRow>>('GET', J(`/cases${qs(q)}`)),
		case: (id: string) => req<T.CaseDetail>('GET', J(`/cases/${seg(id)}`)),
		/** The tenant's LLM providers, the qualify/gates bindings and the platform default (spec §2). */
		llmProviders: () => req<LlmProviders>('GET', '/api/providers'),
		/** `token` is write-only. */
		createLlmProvider: (input: LlmProviderInput) => req<LlmProvider>('POST', '/api/providers', input),
		/** `token` absent = keep the stored one, `""` = clear it. */
		updateLlmProvider: (id: string, input: LlmProviderInput) =>
			req<LlmProvider>('PUT', `/api/providers/${seg(id)}`, input),
		/** 409 `provider_in_use` while a use is bound to it. */
		deleteLlmProvider: (id: string) => req<void>('DELETE', `/api/providers/${seg(id)}`),
		testLlmProvider: (id: string) => req<LlmProviderTest>('POST', `/api/providers/${seg(id)}/test`),
		/** Tests unsaved form values. 422 `token_required_for_new_base_url`: a stored token never goes to a new URL. */
		testLlmProviderDraft: (input: LlmProviderTestInput) =>
			req<LlmProviderTest>('POST', '/api/providers/test', input),
		bindLlmUse: (use: LlmUse, input: LlmUseInput) =>
			req<LlmUseBinding>('PUT', `/api/providers/uses/${seg(use)}`, input),
		/** The tenant's LLM settings (tenant_admin): `external_llm_allowed` gates research runs only. */
		setLlmSettings: (input: LlmSettings) => req<LlmSettings>('PUT', '/api/providers/settings', input),
		/** strata_rag / strata_ingest registrations, proxied by carebus-api. */
		providerSlots: () => req<ProviderSlots>('GET', '/api/providers/slots'),
		/** `api_key` is write-only; omit it to keep the stored key (same base_url only). */
		saveProviderSlot: (capability: string, input: ProviderInput) =>
			req<ProviderSlots>('PUT', `/api/providers/slots/${seg(capability)}`, input),
		removeProviderSlot: (capability: string) =>
			req<ProviderSlots>('DELETE', `/api/providers/slots/${seg(capability)}`),
		// routes::signups (super_admin; 503 signup_not_configured while the feature is off). `before` is the `next`
		// cursor of the previous page.
		signups: (q: { status?: SignupListStatus; limit?: number; before?: string | null } = {}) =>
			req<SignupPage>('GET', `/api/signup-approvals${qs(q)}`),
		signup: (id: string) => req<Signup>('GET', `/api/signup-approvals/${seg(id)}`),
		/** The journaled approve. 409 signup_not_pending / approval_in_progress / identity_exists …, 502 `<step code>` with `error.steps`. */
		approveSignup: (id: string, tenantName?: string) =>
			req<ApproveResult>(
				'POST',
				`/api/signup-approvals/${seg(id)}/approve`,
				tenantName ? { tenantName } : {},
			),
		/** The reject; its `reject_repair` says what an aborted approve's leftovers became. */
		rejectSignup: (id: string, reason?: string) =>
			req<Signup>('POST', `/api/signup-approvals/${seg(id)}/reject`, reason ? { reason } : {}),
		/** Every failed step of an approved signup runs again (409 signup_not_approved otherwise). */
		retrySignup: (id: string) => req<RetryResult>('POST', `/api/signup-approvals/${seg(id)}/retry`),
		/** Remove an APPROVED signup end to end; `confirm` = its `tenant_slug`. The answer is the signup with `removal`. */
		removeSignup: (id: string, confirm: string) =>
			req<Signup>('POST', `/api/signup-approvals/${seg(id)}/remove`, { confirm }),
		/** A pending / blocked / failed removal runs again. */
		retryRemoval: (id: string) => req<Signup>('POST', `/api/signup-approvals/${seg(id)}/removal/retry`),
		/** The pre-approved invite: the signup recorded, then the same approve. */
		inviteSignup: (input: InviteInput) => req<InviteResult>('POST', '/api/signup-approvals/invite', input),
		connectors: (layer?: T.Layer) => req<T.Paged<T.Connector>>('GET', J(`/connectors${layerQ(layer)}`)),
		createConnector: async (c: T.ConnectorInput) => {
			guardSecretRef(c.secret_ref);
			return req<T.Connector>('POST', J('/connectors'), c);
		},
		saveConnector: async (c: T.ConnectorInput) => {
			guardSecretRef(c.secret_ref);
			return req<T.Connector>('PUT', J(`/connectors/${seg(c.id)}`), c);
		},
		testConnector: (id: string) => req<T.ConnectorProbe>('POST', J(`/connectors/${seg(id)}/test`)),
		contextSpaces: (layer?: T.Layer) =>
			req<T.Paged<T.ContextSpace>>('GET', J(`/context-spaces${layerQ(layer)}`)),
		// routes::ontology::put_space: 200 with the effective space (a tenant override); 400 invalid_body /
		// unknown_entity / invalid_key, 404 when the job has no such space.
		saveContextSpace: (key: string, patch: T.ContextSpaceInput) =>
			req<T.ContextSpace>('PUT', J(`/context-spaces/${seg(key)}`), patch),
		// routes::ontology::entities: the effective entities; includeHidden adds the hidden ones (`hidden: true`).
		entities: (opts: { includeHidden?: boolean; layer?: T.Layer } = {}) =>
			req<T.Paged<T.OntologyEntity>>(
				'GET',
				J(
					`/ontology/entities${qs({
						include_hidden: opts.includeHidden ? 'true' : undefined,
						layer: opts.layer === 'effective' ? undefined : opts.layer,
					})}`,
				),
			),
		// routes::ontology::create: 201 with the stored entity; 409 entity_exists / duplicate_attribute, 400 invalid_body / invalid_scope.
		createEntity: (e: T.EntityInput) => req<T.OntologyEntity>('POST', J('/ontology/entities'), e),
		saveEntity: (e: T.EntityInput) => req<{ name: string }>('PUT', J(`/ontology/entities/${seg(e.name)}`), e),
		relations: (layer?: T.Layer) =>
			req<T.Paged<T.OntologyRelationItem>>('GET', J(`/ontology/relations${layerQ(layer)}`)),
		knowledgeStores: (layer?: T.Layer) =>
			req<T.Paged<T.KnowledgeStoreItem>>('GET', J(`/knowledge-stores${layerQ(layer)}`)),
		// routes::refinements: 204; 409 refinement_target_missing (hide without a base row), 404 refinement.
		refine: (op: T.RefinementOp, object: T.RefinableObject, key: Record<string, string>) =>
			req<void>('POST', J(`/refinements/${op}`), { object, key }),
		jobSettings: () => req<T.JobSettingsView>('GET', J('/settings')),
		/** A key set to a string overrides, null hides the base value; absent keys are unchanged. */
		saveJobSettings: (patch: Record<string, string | null>) =>
			req<T.JobSettingsView>('PUT', J('/settings'), patch),
		revertJobSetting: (key: string) => req<unknown>('DELETE', J(`/settings/${seg(key)}`)),
		currentRelease: () => req<T.ReleaseReport>('GET', J('/releases/current')),
		releaseConflicts: (status: 'open' | 'all' = 'open') =>
			req<T.Paged<T.ReleaseConflict>>('GET', J(`/release-conflicts${qs({ status })}`)),
		// routes::ontology::create_relation: 201 with the relation; 409 relation_exists, 400 unknown_entity / invalid_body.
		createRelation: (r: T.OntologyRelation) => req<T.OntologyRelation>('POST', J('/ontology/relations'), r),
		// routes::ontology::delete_relation: 204; 404 when absent.
		deleteRelation: (r: Pick<T.OntologyRelation, 'subject' | 'predicate' | 'object'>) =>
			req<void>('DELETE', J(`/ontology/relations/${seg(r.subject)}/${seg(r.predicate)}/${seg(r.object)}`)),
		procedures: () => req<T.Paged<T.ProcedureSummary>>('GET', J('/procedures')),
		procedure: (name: string, version: string) =>
			req<T.ProcedureDetail>('GET', J(`/procedures/${seg(name)}/versions/${seg(version)}`)),
		// routes::procedures::create: 201 with the stored version; 400 invalid_plan / steps_do_not_match_plan /
		// invalid_version / unknown_context_space, 409 version_exists, 422 secret_value_rejected.
		createProcedure: (p: T.ProcedureInput) => req<T.ProcedureDetail>('POST', J('/procedures'), p),
		// routes::procedures::transition: 200 with the detail; 409 invalid_transition / needs_two_reviewers (a publish
		// with fewer than two distinct reviewers; nothing is written) / space_already_published, 400 invalid_plan.
		setProcedureStatus: (name: string, version: string, to: T.VersionStatus) =>
			req<T.ProcedureDetail>('POST', J(`/procedures/${seg(name)}/versions/${seg(version)}/status`), { to }),
		// routes::procedures::approve: adds the caller to a CANARY version's reviewers, 200 with the updated detail;
		// 409 not_in_review (not CANARY) / already_approved.
		approveProcedure: (name: string, version: string) =>
			req<T.ProcedureDetail>('POST', J(`/procedures/${seg(name)}/versions/${seg(version)}/approve`)),
		stores: () => req<T.Paged<T.StoreNode>>('GET', '/api/knowledge/stores'),
		storeDocuments: (id: string, before?: string) =>
			req<T.Paged<T.StoreDocument>>('GET', `/api/knowledge/stores/${seg(id)}/documents${qs({ before })}`),
		query: (body: {
			query: string;
			route_mode: 'embed' | 'laya' | 'hybrid';
			top_k: number;
			beam: number;
			min_score: number;
		}) => req<T.QueryResult>('POST', '/api/knowledge/query', body),
		sources: (q: { limit?: number; before?: string } = {}) =>
			req<T.Paged<T.Source>>('GET', J(`/sources${qs(q)}`)),
		source: (id: string) => req<T.Source>('GET', J(`/sources/${seg(id)}`)),
		createSource: async (s: T.SourceInput) => {
			guardConfig(s.config);
			return req<T.Source>('POST', J('/sources'), s);
		},
		saveSource: async (id: string, s: T.SourceInput) => {
			guardConfig(s.config);
			return req<T.Source>('PUT', J(`/sources/${seg(id)}`), s);
		},
		deleteSource: (id: string, purge: boolean) =>
			req<T.SourceDeleted>('DELETE', J(`/sources/${seg(id)}${qs({ purge })}`)),
		/** A saved source as stored, or with `edits` (unsaved form changes) for this request only. */
		testSource: async (id: string, edits?: T.SourceTestOverride) => {
			if (edits?.config) guardConfig(edits.config);
			return req<T.ApiSample>('POST', J(`/sources/${seg(id)}/test`), edits);
		},
		/** An unsaved api source; nothing is stored. */
		testUnsavedSource: async (t: T.UnsavedSourceTest) => {
			guardConfig(t.config);
			return req<T.ApiSample>('POST', J('/sources/test'), t);
		},
		/** A CSV or XLSX; `sheet` picks an XLSX sheet (the first when blank); `headerRow` false reads row 1 as data. */
		previewColumns: (file: File, sheet?: string, headerRow?: boolean) => {
			const fd = filesForm([file]);
			if (sheet?.trim()) fd.append('sheet', sheet.trim());
			if (headerRow !== undefined) fd.append('header_row', String(headerRow));
			return req<T.ColumnPreview>('POST', J('/sources/preview-columns'), fd);
		},
		/** A file source sends its files; every other type sends no body. */
		resync: (id: string, files?: File[]) =>
			req<T.SourceRun>('POST', J(`/sources/${seg(id)}/resync`), files ? filesForm(files) : undefined),
		sourceRuns: (id: string, q: { limit?: number; before?: string } = {}) =>
			req<T.Paged<T.SourceRun>>('GET', J(`/sources/${seg(id)}/runs${qs(q)}`)),
		sourceRun: (id: string, runId: string) =>
			req<T.SourceRun>('GET', J(`/sources/${seg(id)}/runs/${seg(runId)}`)),
		ingestJobs: (q: { store_id?: string; status?: string; limit?: number; before?: string } = {}) =>
			req<T.Paged<T.IngestJob>>('GET', `/api/ingest/jobs${qs(q)}`),
		ingestJob: (id: string) => req<T.IngestJob>('GET', `/api/ingest/jobs/${seg(id)}`),
		retryJob: (id: string) => req<T.IngestJob>('POST', `/api/ingest/jobs/${seg(id)}/retry`),
		/** A queued job is `cancelled` at once, a running one `cancelling` until its step ends; 409 once ended. */
		cancelJob: (id: string) => req<T.IngestJob>('POST', `/api/ingest/jobs/${seg(id)}/cancel`),
		/**
		 * An ended job and its staged files; its documents stay in strata_rag. 409 while it still runs, 409
		 * `child_of_parent_job` (with `parent_job_id`) for a crawl/sitemap page job; 202 `{job_id, status: "deleting"}`
		 * when strata_ingest finishes it in the background.
		 */
		deleteJob: (id: string) =>
			req<T.IngestJobDeleted | T.IngestJobDeleting>('DELETE', `/api/ingest/jobs/${seg(id)}`),
		createStore: (b: T.StoreInput) => req<T.StoreNode>('POST', '/api/knowledge/stores', b),
		deleteStore: (id: string, recursive: boolean) =>
			req<void>('DELETE', `/api/knowledge/stores/${seg(id)}${qs({ recursive })}`),
		deleteDocument: (id: string) => req<void>('DELETE', `/api/knowledge/documents/${seg(id)}`),
		/** The tenant's golden questions (at most 200, one page). */
		evalQuestions: () => req<T.Paged<T.EvalQuestion>>('GET', '/api/knowledge/eval/questions'),
		// routes::knowledge_eval: 201; 409 question_exists (same query, any case), 422 invalid_input / too_many_questions.
		createEvalQuestion: (q: T.EvalQuestionInput) =>
			req<T.EvalQuestion>('POST', '/api/knowledge/eval/questions', q),
		/** A key left out keeps its value; `null` clears it (`[]` clears an id list). */
		updateEvalQuestion: (id: string, patch: Partial<T.EvalQuestionInput>) =>
			req<T.EvalQuestion>('PATCH', `/api/knowledge/eval/questions/${seg(id)}`, patch),
		deleteEvalQuestion: (id: string) => req<void>('DELETE', `/api/knowledge/eval/questions/${seg(id)}`),
		/**
		 * A CSV with a header row (`query` plus expectation columns, ids `;`-separated), sent as text/csv whatever
		 * type the browser gave the file. All or nothing: 422 invalid_input lists every bad line.
		 */
		importEvalQuestions: (csv: Blob | string) =>
			req<T.EvalImportResult>(
				'POST',
				'/api/knowledge/eval/questions/import',
				new Blob([csv], { type: 'text/csv' }),
			),
		// 202 the run's summary, `running` (scored in the background: poll evalRun); 409 eval_not_ready (lists what is
		// missing) / eval_running (one at a time per tenant), 404 a store or question gone.
		runEval: (input: T.EvalRunInput) => req<T.EvalRunStarted>('POST', '/api/knowledge/eval/runs', input),
		/** Newest first, as summaries (`question_count`, no questions or results). */
		evalRuns: (q: { limit?: number; before?: string } = {}) =>
			req<T.Paged<T.EvalRunSummary>>('GET', `/api/knowledge/eval/runs${qs(q)}`),
		/** One run with its question snapshot and per-question `results`. */
		evalRun: (id: string) => req<T.EvalRun>('GET', `/api/knowledge/eval/runs/${seg(id)}`),
		tools: () => req<T.Paged<T.ToolInfo>>('GET', J('/tools')),
		// routes::research (plan 18): any member of the tenant. 201 the queued run; 400 invalid_body, 422 unknown_store
		// (a store that is not one of this job's knowledge stores).
		createResearchRun: (input: ResearchRunInput) => req<ResearchRun>('POST', J('/research-runs'), input),
		/** Newest first; `question` is cut to its first 200 characters. */
		researchRuns: (q: { status?: RunStatus; matter_id?: string; limit?: number; before?: string } = {}) =>
			req<T.Paged<ResearchRunSummary>>('GET', J(`/research-runs${qs(q)}`)),
		/** The run with its steps, claims and citations; `stepsFull` adds each step's request and response. */
		researchRun: (id: string, opts: { stepsFull?: boolean } = {}) =>
			req<ResearchRunDetail>(
				'GET',
				J(`/research-runs/${seg(id)}${qs({ include: opts.stepsFull ? 'steps_full' : undefined })}`),
			),
		/** Asks the runner to stop (a queued run is cancelled at once); 409 run_finished once it has ended. */
		cancelResearchRun: (id: string) => req<ResearchRun>('POST', J(`/research-runs/${seg(id)}/cancel`)),
		// routes::mcp_tasks: staff only (a person in the tenant). answerMcpTask: 422 invalid_answer; 409 not_waiting /
		// answered_elsewhere / task_lost. cancelMcpTask: 409 task_finished.
		mcpTasks: (state: TaskListState = 'open') => req<TaskList>('GET', J(`/mcp-tasks${qs({ state })}`)),
		mcpTask: (id: string) => req<TaskView>('GET', J(`/mcp-tasks/${seg(id)}`)),
		answerMcpTask: (id: string, body: AnswerBody) =>
			req<AnswerResult>('POST', J(`/mcp-tasks/${seg(id)}/answers`), body),
		cancelMcpTask: (id: string) => req<CancelResult>('POST', J(`/mcp-tasks/${seg(id)}/cancel`), {}),
		// routes::legal_matters (migration 0026): the matters the caller may see (members; a tenant_admin sees all).
		// `mine` = only the caller's, `open` = not closed. At most 500, `next` is always null.
		legalMatters: (q: { mine?: boolean; open?: boolean } = {}) =>
			req<T.Paged<LegalMatter>>('GET', J(`/legal-matters${qs(q)}`)),
		/** 201; the caller becomes the lead. 409 reference_taken, 422 not_a_tenant_user. */
		createLegalMatter: (input: LegalMatterInput) => req<LegalMatter>('POST', J('/legal-matters'), input),
		legalMatter: (id: string) => req<LegalMatterDetail>('GET', J(`/legal-matters/${seg(id)}`)),
		/** Lead or tenant_admin. */
		updateLegalMatter: (id: string, patch: LegalMatterPatch) =>
			req<LegalMatter>('PATCH', J(`/legal-matters/${seg(id)}`), patch),
		/** Lead or tenant_admin; 409 matter_has_runs / matter_has_stores. */
		deleteLegalMatter: (id: string) => req<void>('DELETE', J(`/legal-matters/${seg(id)}`)),
		/** Lead or tenant_admin (not for themselves); 422 unknown_user, 409 last_lead. */
		setMatterMember: (id: string, sub: string, role: MatterRole) =>
			req<{ user_sub: string; role: MatterRole }>('PUT', J(`/legal-matters/${seg(id)}/members/${seg(sub)}`), {
				role,
			}),
		removeMatterMember: (id: string, sub: string) =>
			req<void>('DELETE', J(`/legal-matters/${seg(id)}/members/${seg(sub)}`)),
		/** Lead only; 422 unknown_store, 409 matter_closed / store_in_other_matter. */
		attachMatterStore: (id: string, store: string) =>
			req<{ store_id: string }>('PUT', J(`/legal-matters/${seg(id)}/stores/${seg(store)}`)),
		detachMatterStore: (id: string, store: string) =>
			req<void>('DELETE', J(`/legal-matters/${seg(id)}/stores/${seg(store)}`)),
		audit: (q: {
			q?: string;
			actor_type?: string;
			status?: string;
			resource_type?: string;
			limit?: number;
			before?: string;
		}) => req<T.Paged<T.AuditRow>>('GET', `/api/audit${qs(q)}`),
		startConversation: () => req<T.ConversationStart>('POST', J('/conversations')),
		sendMessage: (id: string, text: string) =>
			req<T.MessageTurn>('POST', J(`/conversations/${seg(id)}/messages`), { text }),
		conversation: (id: string) => req<T.Conversation>('GET', J(`/conversations/${seg(id)}`)),
		demoRecords: (table: T.DemoTable, q: { q?: string; limit?: number; offset?: number }) =>
			req<T.DemoTablePage>('GET', J(`/demo/records/${table}${qs(q)}`)),
		demoRecord: (table: T.DemoTable, id: string) =>
			req<T.DemoRecord>('GET', J(`/demo/records/${table}/${seg(id)}`)),
		/** `changes` holds only editable columns (plan 02 answers 422 for any other). */
		saveDemoRecord: (table: T.DemoTable, id: string, changes: T.DemoRecord) =>
			req<T.DemoRecord>('PUT', J(`/demo/records/${table}/${seg(id)}`), changes),
		demoReset: (seed: number) => req<unknown>('POST', J('/demo/reset'), { seed }),
		demoScenarios: () => req<T.ScenarioList>('GET', J('/demo/scenarios')),
		runScenario: (name: string) =>
			req<{ applied: string; changes: unknown[] }>('POST', J(`/demo/scenarios/${seg(name)}`)),
		faults: () => req<T.FaultState>('GET', J('/demo/faults')),
		setFault: (f: T.FaultInput) => req<unknown>('PUT', J('/demo/faults'), f),
	};
}

export type Api = ReturnType<typeof createClient>;
