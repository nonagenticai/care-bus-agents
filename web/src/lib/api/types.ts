// Mirror of carebus-api's response structs (plan 05) — overview D2. Lists are Paged<T> (D1).
// tests/unit/contract.test.ts checks this file against the JSON fixtures plan 05's tests write.
/** A context space's `area` (department). Configuration, not a fixed set: see $lib/areas. */
export type Area = string;
export type Bus = 'customer' | 'worker' | 'tool' | 'dead' | 'other';
export type Outcome = 'completed' | 'not_eligible' | 'failed';
/** `GET /api/cases?outcome=`: an outcome, or `open` (still running, outcome IS NULL). */
export const CASE_FILTERS = ['open', 'completed', 'not_eligible', 'failed'] as const;
export type CaseFilter = (typeof CASE_FILTERS)[number];
export type ModelUse = 'generation' | 'routing' | 'embeddings' | 'rerank' | 'vision';
export type Paged<T> = { items: T[]; next: string | null };

// GET /health (no auth)
export interface Health {
	redis: 'ok' | 'down';
	db: 'ok' | 'down';
}
// routes::tenancy::me (GET /api/me). `tenant` is the caller's home tenant (null when unprovisioned or for a
// super_admin without one); `active_tenant` is the tenant a request with the same X-Active-Tenant-Id would act in.
export type TenantRole = 'tenant_admin' | 'tenant_user';
export interface Me {
	sub: string;
	actor: string;
	super_admin: boolean;
	release_writer: boolean;
	tenant: null | { id: string; name: string; role: TenantRole; status: 'active' | 'suspended' };
	active_tenant: string | null;
}
// routes::tenancy::jobs (GET /api/jobs items): the active tenant's jobs; `on_bus` = agents serve it.
export interface JobSummary {
	key: string;
	label: string;
	settings: { preamble?: string; actor?: string; stance?: string };
	on_bus: boolean;
}
// Every route's error body: {"error":{"code","message"}}
export interface ErrorBody {
	error: { code: string; message: string };
}

// carebus::bus::Envelope
export interface Envelope {
	id: string;
	topic: string;
	from: string;
	correlation_id: string;
	published_at_ms: number;
	payload: unknown;
	/** `name@version` the case runs; only on the orchestrator's work.requested / request.resolved. */
	procedure?: string;
	/** The (tenant, job) scope the envelope was published in, stamped by the scoped bus (W3 plan 13). */
	tenant_id: string | null;
	job: string | null;
}
// redis_admin::Entry (GET /api/streams/{key}/entries, GET /api/dead-letters, SSE `event: bus`,
// CaseDetail.trace). `stream` is the unprefixed key (`bus:dead` for dead letters).
export interface Entry {
	stream: string;
	entry_id: string;
	envelope: Envelope;
}
// SSE `event: bus` and CaseDetail.trace: an Entry plus the consumer group the entry is FOR
// (carebus Streams::consumer_group) and whether that group has XACKed it. Both are null when no group
// is the recipient (dead letters, unknown topics). On SSE `acked` is the publish-time value (false);
// the recipient's XACK follows as `event: ack`. The trace recomputes it from XINFO GROUPS + XPENDING.
export interface BusEvent extends Entry {
	consumer_group: string | null;
	acked: boolean | null;
}
// SSE `event: ack`: the recipient group XACKed this entry (carebus Bus::mirror_ack). Lossy like
// every Pub/Sub frame; a reconnect rebuilds ack state from CaseDetail.trace.
export interface BusAck {
	stream: string;
	entry_id: string;
	consumer_group: string;
	correlation_id: string;
}
// POST /api/conversations (201). Name and tier come from the demo customers table; null when that
// lookup failed (the conversation still starts).
export interface ConversationStart {
	conversation_id: string;
	customer_id: string;
	customer_name: string | null;
	customer_tier: string | null;
}
// carebus::chat::ChatEvent (SSE `event: chat`, Conversation.messages). `case_id` is
// `skip_serializing_if = "Option::is_none"`: the key is absent, never null. Only the agent reply
// that published a case carries it.
export interface ChatEvent {
	conversation_id: string;
	role: 'customer' | 'agent' | 'system';
	text: string;
	case_id?: string;
	at_ms: number;
}
// GET /api/conversations/{id}
export interface Conversation {
	conversation_id: string;
	customer_id: string;
	messages: ChatEvent[];
	case_ids: string[];
}
// POST /api/conversations/{id}/messages: 200 with the turn, or 202 `queued` (another turn is running; the server
// qualifies this text with everything else queued and the reply arrives as a `chat` event; `text` is "").
export interface MessageTurn {
	turn: 'ask' | 'unclear' | 'published' | 'queued';
	text: string;
	case_id: string | null;
}

// routes::overview: day(), probe(), attention
export interface DayStats {
	cases: number;
	completed: number;
	not_eligible: number;
	failed: number;
	open: number;
	median_ms: number | null;
}
export type DependencyStatus = 'ok' | 'down' | 'unconfigured';
export interface DependencyHealth {
	name: string;
	status: DependencyStatus;
	detail: string | null;
}
export interface Attention {
	severity: 'err' | 'warn';
	title: string;
	page: string;
}
export interface Overview {
	today: DayStats;
	week_ago: DayStats;
	health: DependencyHealth[];
	attention: Attention[];
}

// redis_admin::GroupInfo / StreamInfo
export interface GroupInfo {
	name: string;
	consumers: number;
	pending: number;
	lag: number | null;
	last_delivered_id: string;
}
export interface StreamInfo {
	key: string;
	bus: Bus;
	area: Area | null;
	length: number;
	last_entry_id: string | null;
	groups: GroupInfo[];
}
// routes::agents::AgentView
export type AgentKind = 'front' | 'coordination' | 'worker' | 'bridge';
export interface AgentView {
	id: string;
	name: string;
	kind: AgentKind;
	group: string;
	tool: string | null;
	consumers: number;
	pending: number;
	lag: number;
	streams: string[];
	status: 'idle' | 'ok' | 'warn';
	model_routes: ModelUse[];
}
// A dead letter is an Entry on bus:dead whose envelope.payload is this (crates/carebus/src/bus.rs dead_letter).
export interface DeadLetterPayload {
	stream: string;
	entry_id: string;
	deliveries: number;
	reason: string;
	original: Envelope;
}

// routes::cases::CaseRow and GET /api/cases/{id}
export interface CaseRow {
	case_id: string;
	conversation_id: string | null;
	customer_id: string;
	request_type: string;
	area: Area;
	context_space: string;
	outcome: Outcome | null;
	reason: string | null;
	work_items: number;
	retries: number;
	started_at: string;
	resolved_at: string | null;
	duration_ms: number | null;
	/** `name@version` the case ran (stamped on request.resolved); null while open and for older cases. */
	procedure: string | null;
}
export interface CaseDetail {
	case: CaseRow;
	hash: Record<string, unknown>;
	trace: BusEvent[];
}

// POST /api/connectors/{id}/test: always sends `detail` (plan 05 connectors.rs).
export interface ProbeResult {
	ok: boolean;
	status: number | null;
	latency_ms: number;
	detail: string;
}
export type ConnectorProbe = ProbeResult;

// W3 plan 11: every config item says where it comes from. `origin` 'base' = the job's current release (read-only),
// 'tenant' = this tenant's refinement; `overridden` = a tenant row whose key also has a base row; `hidden` = a
// tombstone (only listed with ?layer=tenant, or ?include_hidden=true for entities).
export type Origin = 'base' | 'tenant';
export interface Overlay {
	origin: Origin;
	overridden: boolean;
	hidden: boolean;
}

// routes::connectors: jsonb_build_object of the listed columns + mappings (no created_at/updated_at)
export interface Mapping {
	backend_field: string;
	attribute: string;
}
export interface Connector extends Overlay {
	id: string;
	name: string;
	system: string;
	/** null only on a base slot whose endpoint the tenant has not supplied yet. */
	base_url: string;
	auth_type: 'oauth2_client_credentials' | 'api_key' | 'mtls_api_key' | 'none';
	secret_ref: string | null;
	timeout_ms: number;
	rate_limit: string | null;
	health: 'unknown' | 'ok' | 'down';
	health_checked_at: string | null;
	mappings: Mapping[];
}
// routes::connectors::ConnectorInput (POST/PUT body)
export type ConnectorInput = Pick<
	Connector,
	| 'id'
	| 'name'
	| 'system'
	| 'base_url'
	| 'auth_type'
	| 'secret_ref'
	| 'timeout_ms'
	| 'rate_limit'
	| 'mappings'
>;

// routes::ontology
export interface ContextSpace extends Overlay {
	key: string;
	area: Area;
	label: string;
	request_type: string;
	required: string[];
	entities: string[];
	procedure: string;
	topic_prefix: string;
	streams: { customer: string; worker_results: string; worker: string[] };
}
/**
 * routes::ontology::put_space (PUT /api/jobs/{job}/context-spaces/{key}): the fields a tenant may override on a
 * base context space; absent fields are unchanged. `key`, `area`, `request_type` and `procedure` route the space
 * and come with the release only (400 invalid_body).
 */
export interface ContextSpaceInput {
	label?: string;
	required?: string[];
	entities?: string[];
}
export interface Attribute {
	name: string;
	data_type: string;
	pii: boolean;
	synonyms: string[];
}
export type Scope = 'generic' | 'industry' | 'industry_dept';
export interface OntologyEntity extends Overlay {
	name: string;
	scope: Scope;
	industry: string | null;
	department: string | null;
	label: string;
	synonyms: string[];
	attributes: Attribute[];
}
// routes::ontology::EntityInput (POST/PUT body)
export type EntityInput = Omit<OntologyEntity, keyof Overlay>;
// routes::ontology::CARDINALITIES (subject side : object side)
export type Cardinality = '1:1' | '1:n' | 'n:1' | 'n:m';
// The POST /api/ontology/relations body and its 201 answer
export interface OntologyRelation {
	subject: string;
	predicate: string;
	object: string;
	cardinality: Cardinality;
}
// GET /api/ontology/relations items
export interface OntologyRelationItem extends OntologyRelation, Overlay {}

// routes::procedures (procedure_management vocabulary)
export type VersionStatus = 'DRAFT' | 'CANARY' | 'PUBLISHED' | 'ARCHIVED' | 'DISCARDED';
// GET /api/procedures items: `steps` is a count here
export interface ProcedureSummary extends Overlay {
	/** A base PUBLISHED version that a tenant PUBLISHED version (same name or context space) replaces. */
	shadowed: boolean;
	name: string;
	version: string;
	version_status: VersionStatus;
	context_space: string;
	domain: string;
	steps: number;
	reviewers: string[];
	updated_at: string;
	plan: unknown;
	/** Resolved cases this version ran, started in the last 7 days. */
	runs_7d: number;
	/** (completed + not_eligible) / runs_7d; null when runs_7d = 0. */
	success_rate: number | null;
}
export interface ProcedureStep {
	step_number: number;
	phase: number;
	name: string;
	action_type: string;
	worker: string;
	mcp_tool: string;
	performed_by: string;
	preconditions: string[];
	postconditions: string[];
	parameters: Record<string, unknown>;
	gate: boolean;
	/** Whether the step's tool call changes backend state; absent: the agents' SIDE_EFFECT_TOOLS list decides. */
	side_effect?: boolean;
}
// GET /api/procedures/{name}/versions/{version} and GET /api/procedures/{name}?status=PUBLISHED:
// to_jsonb(procedures) + steps
export interface ProcedureDetail {
	origin: Origin;
	overridden: boolean;
	shadowed: boolean;
	id: number;
	name: string;
	version: string;
	version_status: VersionStatus;
	context_space: string;
	domain: string;
	description: string | null;
	pddl_domain: string | null;
	tool_requirements: string[];
	success_criteria: string[];
	reviewers: string[];
	plan: unknown;
	created_at: string;
	updated_at: string;
	steps: ProcedureStep[];
	// This version's audit trail, oldest first (routes::procedures::load). Empty for migration-seeded versions.
	history: ProcedureHistoryEntry[];
}
// One audit_events row for `name@version`: action `create`, `approve` or `transition`. Refused requests write no
// row; `replaced_by` is set on the archive a later publish or roll-back caused.
export interface ProcedureHistoryEntry {
	at: string;
	actor: string;
	action: string;
	ok: boolean;
	from: VersionStatus | null;
	to: VersionStatus | null;
	reason: string | null;
	replaced_by: string | null;
}
// POST /api/procedures body (routes::procedures::NewProcedure). `steps` must mirror the plan one-to-one.
export interface ProcedureInput {
	name: string;
	version: string;
	context_space: string;
	description?: string | null;
	pddl_domain?: string | null;
	plan: unknown;
	steps: ProcedureStepInput[];
}
export type ProcedureStepInput = Pick<
	ProcedureStep,
	'step_number' | 'phase' | 'name' | 'action_type' | 'worker' | 'mcp_tool' | 'performed_by'
> &
	Partial<Pick<ProcedureStep, 'preconditions' | 'postconditions' | 'parameters' | 'gate' | 'side_effect'>>;

// strata_rag shapes, normalized to Paged by the knowledge proxy (overview, F14)
export interface StoreNode {
	id: string;
	parent_id: string | null;
	kind: 'router' | 'leaf';
	name: string;
	description: string;
	token_count: number;
	chunk_count: number;
	children: string[];
}
// strata_rag document item (passed through by the documents proxy)
export interface StoreDocument {
	id: string;
	store_id: string;
	title: string | null;
	source: string | null;
	token_count: number;
	chunk_count: number;
	content_hash: string;
	created_at: string;
}
// strata_rag POST /api/query, passed through unchanged. Scores are Option<f32> serialized as null.
export interface QueryScores {
	dense: number | null;
	lexical: number | null;
	rerank: number | null;
}
/** strata_rag `query::QueryResult`. */
export interface QueryHit {
	chunk_id: string;
	document_id: string;
	store_path: string[];
	/** The chunk's position in its document. */
	ord: number;
	text: string;
	/** First / last page the chunk spans; `null` when the document has no pages. */
	page_start: number | null;
	page_end: number | null;
	/** The document's `source` label and `title`, for citations. */
	source: string;
	title: string | null;
	scores: QueryScores;
	/** The document's provenance (the source and run that ingested it). */
	source_id: string | null;
	source_run: string | null;
	metadata: Record<string, unknown>;
}
export interface QueryRouting {
	mode_used: string;
	leaves: string[];
	laya_consulted: boolean;
	laya_fallback?: string | null;
}
export interface QueryTimings {
	embed: number;
	route: number;
	search: number;
	rerank: number;
	total: number;
}
export interface QueryResult {
	results: QueryHit[];
	routing: QueryRouting;
	timings_ms: QueryTimings;
}

// routes::tools
export interface ToolInfo {
	name: string;
	description: string | null;
	input_schema: unknown;
	calls_24h: number;
	error_rate: number | null;
	p95_ms: number | null;
}
// routes::analytics
export interface Analytics {
	days: {
		day: string;
		completed: number;
		not_eligible: number;
		failed: number;
		open: number;
	}[];
	by_type: {
		request_type: string;
		cases: number;
		completed: number;
		not_eligible: number;
		failed: number;
		retries: number;
		p50_ms: number | null;
	}[];
	latency: { tool: string; p50_ms: number | null; p95_ms: number | null }[];
	previous: { cases: number; completed: number; not_eligible: number; failed: number };
}
// audit::AuditRow (nonagenticai_mcp_server audit_logs columns)
export interface AuditRow {
	id: number;
	timestamp: string;
	actor_id: string;
	actor_type: string;
	action_type: string;
	resource_type: string;
	resource_id: string;
	status: 'success' | 'failure';
	details: Record<string, unknown>;
	request_id: string | null;
	ip_address: string | null;
}

// plan 02 shapes, passed through by /api/demo/*
export type DemoTable =
	| 'customers'
	| 'orders'
	| 'shipments'
	| 'payments'
	| 'communications'
	| 'refunds'
	| 'rmas'
	| 'warranty_claims';
export type DemoRecord = Record<string, unknown>;
export interface DemoTablePage {
	table: DemoTable;
	primary_key: string;
	editable: string[];
	columns: string[];
	rows: DemoRecord[];
	total: number;
}
export interface Scenario {
	name: string;
	description: string;
}
export interface ScenarioList {
	scenarios: Scenario[];
}
export type FaultMode = 'timeout' | 'error';
export interface FaultState {
	faults: Record<string, { mode: FaultMode; remaining: number }>;
}
export interface FaultInput {
	tool: string;
	mode: FaultMode | 'none';
	count: number;
}

// ---- Sources (carebus-api crate::sources, crate::runs, crate::netguard, crate::tabular) -----------------------
export type SourceType = 'website' | 'api' | 'file' | 'datatable';
export type RunStatus = 'running' | 'completed' | 'failed' | 'unsupported';
// sources::MappingRow
export interface SourceMapping {
	source_field: string;
	entity: string;
	attribute: string;
}
// sources::RunSummary (SourceView.last_run)
export interface RunSummary {
	id: string;
	status: RunStatus;
	started_at: string;
	finished_at: string | null;
	document_count: number;
	error: string | null;
}
// sources::SourceView. Credentials are write-only: only `has_credentials` comes back.
export interface Source {
	id: string;
	name: string;
	type: SourceType;
	config: Record<string, unknown>;
	mapping: SourceMapping[];
	target_store_id: string;
	schedule: string | null;
	enabled: boolean;
	has_credentials: boolean;
	created_at: string;
	updated_at: string;
	updated_by: string;
	last_run: RunSummary | null;
}
// POST/PUT /api/sources body. `credentials` absent = keep, null = clear, {secret} = replace.
export interface SourceInput {
	name: string;
	type: SourceType;
	config: Record<string, unknown>;
	mapping: SourceMapping[];
	target_store_id: string;
	schedule: string | null;
	enabled: boolean;
	credentials?: { secret: string } | null;
}
export interface ReplaceReport {
	deleted: number;
	already_gone: number;
	kept_shared: number;
	failed: string[];
}
// runs::SourceRun
export interface SourceRun {
	id: string;
	source_id: string;
	status: RunStatus;
	started_at: string;
	finished_at: string | null;
	started_by: string;
	ingest_job_ids: string[];
	document_ids: string[];
	error: string | null;
	replaced_run_id: string | null;
	replace_report: ReplaceReport | null;
	/** strata_ingest could not be asked during this read; `status` may be behind. */
	stale: boolean;
	/** While a crawl or sitemap run is followed live: its page jobs. Absent otherwise. */
	children?: ChildrenSummary;
}
// The connection settings a test connection sends (an api source's config.url + config.auth).
export type SourceTestConfig = {
	url: string;
	auth: { type: 'none' | 'bearer' } | { type: 'header'; header_name: string };
};
// POST /api/sources/test: an unsaved api source. The secret is used for that one request; nothing is stored.
export interface UnsavedSourceTest {
	type: 'api';
	config: SourceTestConfig;
	credentials?: { secret: string };
}
// Optional POST /api/sources/{id}/test body: edits tested before they are saved. `credentials` absent = the stored
// secret (only against the saved host), null = none, {secret} = this one for this request only.
export interface SourceTestOverride {
	config?: SourceTestConfig;
	credentials?: { secret: string } | null;
}
// netguard::ApiSample (POST /api/sources/test and /api/sources/{id}/test)
export interface ApiSample {
	ok: boolean;
	status: number | null;
	detail: string;
	sample: unknown;
	fields: string[];
	truncated: boolean;
}
// tabular::ColumnPreview (POST /api/sources/preview-columns)
export interface ColumnPreview {
	format: 'csv' | 'xlsx';
	columns: string[];
	sample_rows: string[][];
	row_count: number;
}
// runs::SourceDeleted
export interface SourceDeleted {
	id: string;
	purged: boolean;
	documents_deleted: number;
	documents_kept: number;
}
/**
 * strata_ingest job states; `cancelling` = asked to stop, its step in flight still finishing (then `cancelled`);
 * `deleting` = an ended job whose DELETE has not finished (a storage failure leaves it there; DELETE again finishes).
 */
export type IngestStatus =
	'queued' | 'running' | 'cancelling' | 'deleting' | 'completed' | 'failed' | 'rejected' | 'cancelled';
// ingest::DeletedJob (DELETE /api/ingest/jobs/{id}): the job is gone, its documents are not
export interface IngestJobDeleted {
	job_id: string;
	/** The status the job had. */
	status: IngestStatus;
	/** The job plus, for a crawl/sitemap parent, its page jobs. */
	deleted_jobs: number;
	deleted_objects: number;
	/** Documents it delivered: still in strata_rag. */
	document_ids: string[];
	documents: 'retained';
}
/** 202 to DELETE /api/ingest/jobs/{id}: strata_ingest finishes the delete in the background; the job reads
 * `deleting` until it is gone (404). */
export interface IngestJobDeleting {
	job_id: string;
	status: 'deleting';
}
// ingest::IngestStep
export interface IngestStep {
	kind: string;
	key: string;
	status: string;
	attempt: number;
	provider: string | null;
	model: string | null;
	usage: unknown;
	cost_micros: number | null;
	error: unknown;
	started_at: string | null;
	finished_at: string | null;
}
// ingest::IngestJob (credentials always stripped by carebus-api)
export interface IngestJob {
	id: string;
	store_id: string;
	source: unknown;
	options: unknown;
	meta: unknown;
	status: IngestStatus;
	error: unknown;
	document_ids: string[];
	created_at: string;
	updated_at: string;
	steps: IngestStep[];
	/** A crawl or sitemap parent job: its page jobs. Absent for an ordinary job. */
	children?: ChildrenSummary;
	child_job_ids?: string[];
}
/** strata_ingest's summary of a parent job's page jobs. */
export interface ChildrenSummary {
	total: number;
	/** Count per job status (queued, running, completed, …). */
	by_status: Record<string, number>;
}
// POST /api/knowledge/stores body
export interface StoreInput {
	parent_id: string | null;
	kind: 'router' | 'leaf';
	name: string;
	description: string;
}

// ---- RAG eval: routes::knowledge_eval over strata_rag /api/eval (strata_rag PR #21) ----

/** The search modes strata_rag scores an eval in. */
export type EvalMode = 'embed' | 'laya' | 'hybrid' | 'flat';
/**
 * A golden question as written (POST/PATCH /api/knowledge/eval/questions, one CSV row). At least one of
 * expected_document / expected_document_ids / expected_chunk_ids / expected_text is required; a result is a hit
 * when it matches ANY of them. expected_leaf (leaf id, path a/b, or unique leaf name) only feeds leaf accuracy.
 */
export interface EvalQuestionInput {
	query: string;
	expected_document?: string | null;
	expected_leaf?: string | null;
	expected_document_ids?: string[];
	expected_chunk_ids?: string[];
	expected_text?: string | null;
}
// strata_rag tenant_eval::Question (GET /api/knowledge/eval/questions items, POST/PATCH answers)
export interface EvalQuestion {
	id: string;
	query: string;
	expected_document: string | null;
	expected_leaf: string | null;
	expected_document_ids: string[];
	expected_chunk_ids: string[];
	expected_text: string | null;
	/** The X-Actor-Id (the user's uuid subject) that created it; null when none was sent. */
	created_by: string | null;
	created_at: string;
	updated_at: string;
}
// POST /api/knowledge/eval/questions/import (CSV or {questions}): all or nothing
export interface EvalImportResult {
	added: number;
	/** The tenant's questions after the import. */
	total: number;
}
/** A question as a run snapshotted it: a later edit or delete does not change an old run. */
export interface EvalRunQuestion {
	/** The stored question's id at run time. */
	id: string | null;
	query: string;
	expected_document: string | null;
	expected_leaf: string | null;
	expected_document_ids: string[];
	expected_chunk_ids: string[];
	expected_text: string | null;
}
// strata_rag eval::ModeReport: recall@k is the share of questions with a hit in the top k (0..1).
export interface EvalModeReport {
	mode: EvalMode;
	n: number;
	recall_at_1: number;
	recall_at_5: number;
	recall_at_10: number;
	/** Share of routed questions that reached their expected_leaf; null for flat or without expected leaves. */
	leaf_accuracy: number | null;
	laya_consulted: number;
	laya_fallbacks: number;
}
/** One question in one mode. `index` is its position in the run's `questions`. */
export interface EvalResult {
	index: number;
	question_id: string | null;
	mode: EvalMode;
	/** 1-based rank of the first expected result; null = not in the top 10. */
	rank: number | null;
	/** Routing reached expected_leaf; null for flat or without an expected leaf. */
	leaf_hit: boolean | null;
}
/** A run is scored in the background: `running` until it is `completed` (scores) or `failed` (`error`). */
export type EvalRunStatus = 'running' | 'completed' | 'failed';
/** Question × mode searches scored so far. */
export interface EvalProgress {
	done: number;
	total: number;
}
/** What every view of a run carries. While it runs, verdict / finished_at / took_ms / reports are null. */
interface EvalRunHead {
	id: string;
	source: 'tenant' | 'inline';
	/** The store the eval was scoped to; null = the tenant's whole tree. */
	root_id: string | null;
	modes: EvalMode[];
	beam: number;
	min_score: number;
	status: EvalRunStatus;
	/** Why a `failed` run stopped; null otherwise. */
	error: string | null;
	/** Question × mode searches done / to do (strata_rag updates it about once a second). */
	progress: EvalProgress;
	verdict: 'PASS' | 'FAIL' | null;
	started_by: string | null;
	started_at: string;
	finished_at: string | null;
	took_ms: number | null;
	reports: EvalModeReport[] | null;
}
// strata_rag db::eval::EvalRunSummary (GET /api/knowledge/eval/runs items): no questions, no results
export interface EvalRunSummary extends EvalRunHead {
	question_count: number;
}
// strata_rag db::eval::EvalRun (GET /api/knowledge/eval/runs/{id}): the question snapshot and per-question results
export interface EvalRun extends EvalRunHead {
	questions: EvalRunQuestion[];
	results: EvalResult[];
}
// POST /api/knowledge/eval/runs (202): the run's summary, `running`; poll GET …/runs/{id} until it is not
export type EvalRunStarted = EvalRunSummary;
// POST /api/knowledge/eval/runs body: always the tenant's stored questions
export interface EvalRunInput {
	/** Evaluate within this store's subtree; absent = the whole tree. */
	root_id?: string;
	modes?: EvalMode[];
	beam?: number;
	min_score?: number;
	/** Only these stored questions, in this order; absent = all of them. */
	question_ids?: string[];
}

// ---- W3 plan 14: job-scoped admin ----

/** `?layer=` of the overlay list routes (refine.rs `Layer`): one side of the side-by-side view, or the merge. */
export type Layer = 'effective' | 'base' | 'tenant';
/** refine.rs `ObjectKind::as_str`: the `object` of `POST /refinements/{op}` and of a release conflict. */
export type RefinableObject =
	| 'context_space'
	| 'ontology_entity'
	| 'ontology_attribute'
	| 'ontology_relation'
	| 'procedure'
	| 'connector'
	| 'connector_field_mapping'
	| 'knowledge_store'
	| 'job_setting';
export type RefinementOp = 'revert' | 'rebase' | 'hide' | 'unhide';
// routes::tenancy::list_tenants (GET /api/tenants, super_admin only)
export interface TenantSummary {
	id: string;
	name: string;
	status: 'active' | 'suspended';
}
// routes::tenancy::list_users (GET /api/tenants/{tenant_id}/users)
export interface TenantUser {
	sub: string;
	tenant_role: TenantRole;
	display_name: string | null;
	updated_at: string;
	updated_by: string;
}
// PUT /api/tenants/{tenant_id}/users/{sub} body
export interface TenantUserInput {
	tenant_role: TenantRole;
	display_name?: string | null;
}
// routes::knowledge_stores (GET /api/jobs/{job}/knowledge-stores)
export interface KnowledgeStoreItem extends Overlay {
	key: string;
	label: string;
	parent: string | null;
	kind: string;
	areas: string[];
	description: string | null;
	strata_store_id: string | null;
}
/** routes::settings (GET /api/jobs/{job}/settings): `override` maps a key to a string, or null = base value hidden. */
export interface JobSettingsView {
	base: Record<string, string | null>;
	override: Record<string, string | null>;
	effective: Record<string, string | null>;
}
export type ConflictKind =
	'base_changed' | 'base_removed' | 'base_added' | 'parent_missing' | 'base_shadowed';
// routes::releases::ConflictItem (GET /api/jobs/{job}/release-conflicts?status=open|all)
export interface ReleaseConflict {
	id: number;
	kind: ConflictKind;
	object: RefinableObject;
	key: Record<string, string>;
	tenant_row: string;
	base_hash_before: string | null;
	base_hash_after: string | null;
	release_id: string;
	detected_at: string;
	resolved_at: string | null;
	resolution: string | null;
	resolved_by: string | null;
}
// release/apply.rs ReleaseReport (GET /api/jobs/{job}/releases/current)
export interface ReleaseReport {
	release_id: string;
	hash: string;
	applied: boolean;
	previous_release_id: string | null;
	applied_at: string;
	applied_by: string;
	counts: Record<string, number>;
	conflicts: Omit<
		ReleaseConflict,
		'release_id' | 'detected_at' | 'resolved_at' | 'resolution' | 'resolved_by'
	>[];
}
