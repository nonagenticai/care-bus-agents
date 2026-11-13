import type * as G from './gates';
import type * as T from './types';

/** Keys of X that are `?:` (Rust `skip_serializing_if`): absent from the JSON, never null. */
type OptionalKeys<X> = { [K in keyof X]-?: object extends Pick<X, K> ? K : never }[keyof X];
/**
 * `true` = always present; `'optional'` = may be omitted, never null (`k?: T`);
 * `'optional-null'` = may be omitted or null (`k?: T | null`). Must match types.ts exactly.
 */
export type Presence = true | 'optional' | 'optional-null';
type KeyMap<X> = {
	[K in keyof X]-?: K extends OptionalKeys<X> ? (null extends X[K] ? 'optional-null' : 'optional') : true;
};

const k =
	<X>() =>
	<M extends KeyMap<X>>(m: M & Record<Exclude<keyof M, keyof X>, never>) =>
		m;
/** Key map of X, checked at compile time against types.ts (also used by providers.ts). */
export const keyMap = k;

const ENVELOPE = k<T.Envelope>()({
	id: true,
	topic: true,
	from: true,
	correlation_id: true,
	published_at_ms: true,
	payload: true,
	procedure: 'optional',
	tenant_id: true,
	job: true,
});
const ENTRY = k<T.Entry>()({ stream: true, entry_id: true, envelope: true });
const BUS_EVENT = k<T.BusEvent>()({
	stream: true,
	entry_id: true,
	envelope: true,
	consumer_group: true,
	acked: true,
});
const CHAT = k<T.ChatEvent>()({
	conversation_id: true,
	role: true,
	text: true,
	case_id: 'optional',
	at_ms: true,
});
const PROBE = k<T.ProbeResult>()({ ok: true, status: true, latency_ms: true, detail: true });
const DAY = k<T.DayStats>()({
	cases: true,
	completed: true,
	not_eligible: true,
	failed: true,
	open: true,
	median_ms: true,
});
const GROUP = k<T.GroupInfo>()({
	name: true,
	consumers: true,
	pending: true,
	lag: true,
	last_delivered_id: true,
});
const STREAM = k<T.StreamInfo>()({
	key: true,
	bus: true,
	area: true,
	length: true,
	last_entry_id: true,
	groups: true,
});
const AGENT = k<T.AgentView>()({
	id: true,
	name: true,
	kind: true,
	group: true,
	tool: true,
	consumers: true,
	pending: true,
	lag: true,
	streams: true,
	status: true,
	model_routes: true,
});
const CASE_ROW = k<T.CaseRow>()({
	case_id: true,
	conversation_id: true,
	customer_id: true,
	request_type: true,
	area: true,
	context_space: true,
	outcome: true,
	reason: true,
	work_items: true,
	retries: true,
	started_at: true,
	resolved_at: true,
	duration_ms: true,
	procedure: true,
});
const CONNECTOR = k<T.Connector>()({
	id: true,
	name: true,
	system: true,
	base_url: true,
	auth_type: true,
	secret_ref: true,
	timeout_ms: true,
	rate_limit: true,
	health: true,
	health_checked_at: true,
	mappings: true,
	origin: true,
	overridden: true,
	hidden: true,
});
const MAPPING = k<T.Mapping>()({ backend_field: true, attribute: true });
const STEP = k<T.ProcedureStep>()({
	step_number: true,
	phase: true,
	name: true,
	action_type: true,
	worker: true,
	mcp_tool: true,
	performed_by: true,
	preconditions: true,
	postconditions: true,
	parameters: true,
	gate: true,
	side_effect: 'optional',
});
const PROCEDURE_DETAIL = k<T.ProcedureDetail>()({
	origin: true,
	overridden: true,
	shadowed: true,
	id: true,
	name: true,
	version: true,
	version_status: true,
	context_space: true,
	domain: true,
	description: true,
	pddl_domain: true,
	tool_requirements: true,
	success_criteria: true,
	reviewers: true,
	plan: true,
	created_at: true,
	updated_at: true,
	steps: true,
	history: true,
});
const STORE_DOCUMENT = k<T.StoreDocument>()({
	id: true,
	store_id: true,
	title: true,
	source: true,
	token_count: true,
	chunk_count: true,
	content_hash: true,
	created_at: true,
});
const STORE_NODE = k<T.StoreNode>()({
	id: true,
	parent_id: true,
	kind: true,
	name: true,
	description: true,
	token_count: true,
	chunk_count: true,
	children: true,
});
const MAPPING_ROW = k<T.SourceMapping>()({ source_field: true, entity: true, attribute: true });
const RUN_SUMMARY = k<T.RunSummary>()({
	id: true,
	status: true,
	started_at: true,
	finished_at: true,
	document_count: true,
	error: true,
});
const SOURCE = k<T.Source>()({
	id: true,
	name: true,
	type: true,
	config: true,
	mapping: true,
	target_store_id: true,
	schedule: true,
	enabled: true,
	has_credentials: true,
	created_at: true,
	updated_at: true,
	updated_by: true,
	last_run: true,
});
const SOURCE_RUN = k<T.SourceRun>()({
	id: true,
	source_id: true,
	status: true,
	started_at: true,
	finished_at: true,
	started_by: true,
	ingest_job_ids: true,
	document_ids: true,
	error: true,
	replaced_run_id: true,
	replace_report: true,
	stale: true,
	children: 'optional',
});
const REPLACE_REPORT = k<T.ReplaceReport>()({
	deleted: true,
	already_gone: true,
	kept_shared: true,
	failed: true,
});
const INGEST_STEP = k<T.IngestStep>()({
	kind: true,
	key: true,
	status: true,
	attempt: true,
	provider: true,
	model: true,
	usage: true,
	cost_micros: true,
	error: true,
	started_at: true,
	finished_at: true,
});
const INGEST_JOB = k<T.IngestJob>()({
	id: true,
	store_id: true,
	source: true,
	options: true,
	meta: true,
	status: true,
	error: true,
	document_ids: true,
	created_at: true,
	updated_at: true,
	steps: true,
	children: 'optional',
	child_job_ids: 'optional',
});
const SOURCE_TYPES = ['website', 'api', 'file', 'datatable'];
const RUN_STATUSES = ['running', 'completed', 'failed', 'unsupported'];
const INGEST_STATUSES = [
	'queued',
	'running',
	'cancelling',
	'deleting',
	'completed',
	'failed',
	'rejected',
	'cancelled',
];
const ONTOLOGY_RELATION = k<T.OntologyRelation>()({
	subject: true,
	predicate: true,
	object: true,
	cardinality: true,
});
const ONTOLOGY_RELATION_ITEM = k<T.OntologyRelationItem>()({
	subject: true,
	predicate: true,
	object: true,
	cardinality: true,
	origin: true,
	overridden: true,
	hidden: true,
});
const CARDINALITIES: readonly T.Cardinality[] = ['1:1', '1:n', 'n:1', 'n:m'];
const ORIGINS: readonly T.Origin[] = ['base', 'tenant'];

const ROLES = ['customer', 'agent', 'system'];
const VERSION_STATUSES = ['DRAFT', 'CANARY', 'PUBLISHED', 'ARCHIVED', 'DISCARDED'];

type Spec = {
	paged?: true;
	keys?: Record<string, Presence>;
	/** The keys must equal the `columns` array of this other fixture (same table). */
	columnsOf?: string;
	nested?: Record<string, Record<string, Presence>>;
	values?: Record<string, readonly unknown[]>;
};

/**
 * One entry per fixture file in src/lib/api/fixtures/. Every file is written by plan 05's
 * crates/carebus-api/tests/contract_fixtures.rs (UPDATE_FIXTURES=1); none is hand-written.
 */
const CONTEXT_SPACE = k<T.ContextSpace>()({
	key: true,
	area: true,
	label: true,
	request_type: true,
	required: true,
	entities: true,
	procedure: true,
	topic_prefix: true,
	streams: true,
	origin: true,
	overridden: true,
	hidden: true,
});
const CONTEXT_SPACE_STREAMS = { customer: true, worker_results: true, worker: true } as const;
export const FIXTURES: Record<string, Spec> = {
	health: {
		keys: k<T.Health>()({ redis: true, db: true }),
		values: { redis: ['ok', 'down'], db: ['ok', 'down'] },
	},
	overview: {
		keys: k<T.Overview>()({ today: true, week_ago: true, health: true, attention: true }),
		nested: {
			today: DAY,
			health: k<T.DependencyHealth>()({ name: true, status: true, detail: true }),
			attention: k<T.Attention>()({ severity: true, title: true, page: true }),
		},
		values: { 'health.status': ['ok', 'down', 'unconfigured'] },
	},
	agents: {
		paged: true,
		keys: AGENT,
		values: { kind: ['front', 'coordination', 'worker', 'bridge'], status: ['idle', 'ok', 'warn'] },
	},
	agent: {
		keys: AGENT,
		values: { kind: ['front', 'coordination', 'worker', 'bridge'], status: ['idle', 'ok', 'warn'] },
	},
	streams: {
		paged: true,
		keys: STREAM,
		nested: { groups: GROUP },
		values: { bus: ['customer', 'worker', 'tool', 'dead', 'other'] },
	},
	stream: {
		keys: STREAM,
		nested: { groups: GROUP },
		values: { bus: ['customer', 'worker', 'tool', 'dead', 'other'] },
	},
	stream_entries: { paged: true, keys: ENTRY, nested: { envelope: ENVELOPE } },
	dead_letters: {
		paged: true,
		keys: ENTRY,
		nested: {
			envelope: ENVELOPE,
			'envelope.payload': k<T.DeadLetterPayload>()({
				stream: true,
				entry_id: true,
				deliveries: true,
				reason: true,
				original: true,
			}),
		},
	},
	cases: { paged: true, keys: CASE_ROW, values: { outcome: ['completed', 'not_eligible', 'failed', null] } },
	case: {
		keys: k<T.CaseDetail>()({ case: true, hash: true, trace: true }),
		nested: { case: CASE_ROW, trace: BUS_EVENT },
	},
	connectors: {
		paged: true,
		keys: CONNECTOR,
		nested: { mappings: MAPPING },
		values: { health: ['unknown', 'ok', 'down'], origin: ORIGINS },
	},
	connector: {
		keys: CONNECTOR,
		nested: { mappings: MAPPING },
		values: { health: ['unknown', 'ok', 'down'], origin: ORIGINS },
	},
	connector_test: { keys: PROBE },
	context_spaces: {
		paged: true,
		keys: CONTEXT_SPACE,
		nested: { streams: CONTEXT_SPACE_STREAMS },
		values: { origin: ORIGINS },
	},
	// routes::ontology::put_space: the effective space after a tenant override (same shape as a list item).
	context_space: {
		keys: CONTEXT_SPACE,
		nested: { streams: CONTEXT_SPACE_STREAMS },
		values: { origin: ORIGINS },
	},
	ontology_entities: {
		paged: true,
		keys: k<T.OntologyEntity>()({
			name: true,
			scope: true,
			industry: true,
			department: true,
			label: true,
			synonyms: true,
			hidden: true,
			attributes: true,
			origin: true,
			overridden: true,
		}),
		nested: {
			attributes: k<T.Attribute>()({ name: true, data_type: true, pii: true, synonyms: true }),
		},
		values: { scope: ['generic', 'industry', 'industry_dept'], origin: ORIGINS },
	},
	ontology_relations: {
		paged: true,
		keys: ONTOLOGY_RELATION_ITEM,
		values: { cardinality: CARDINALITIES, origin: ORIGINS },
	},
	// POST /api/ontology/relations answers 201 with the relation it stored.
	ontology_relation: { keys: ONTOLOGY_RELATION, values: { cardinality: CARDINALITIES } },
	procedures: {
		paged: true,
		keys: k<T.ProcedureSummary>()({
			name: true,
			version: true,
			version_status: true,
			context_space: true,
			domain: true,
			steps: true,
			reviewers: true,
			updated_at: true,
			plan: true,
			runs_7d: true,
			success_rate: true,
			origin: true,
			overridden: true,
			shadowed: true,
			hidden: true,
		}),
		values: { version_status: VERSION_STATUSES, origin: ORIGINS },
	},
	procedure_version: {
		keys: PROCEDURE_DETAIL,
		nested: { steps: STEP },
		values: { version_status: VERSION_STATUSES },
	},
	procedure_published: {
		keys: PROCEDURE_DETAIL,
		nested: { steps: STEP },
		values: { version_status: ['PUBLISHED'] },
	},
	knowledge_stores: {
		paged: true,
		keys: STORE_NODE,
		values: { kind: ['router', 'leaf'] },
	},
	knowledge_documents: { paged: true, keys: STORE_DOCUMENT },
	knowledge_query: {
		keys: k<T.QueryResult>()({ results: true, routing: true, timings_ms: true }),
		nested: {
			results: k<T.QueryHit>()({
				chunk_id: true,
				document_id: true,
				store_path: true,
				ord: true,
				text: true,
				page_start: true,
				page_end: true,
				source: true,
				title: true,
				scores: true,
				source_id: true,
				source_run: true,
				metadata: true,
			}),
			'results.scores': k<T.QueryScores>()({ dense: true, lexical: true, rerank: true }),
			routing: k<T.QueryRouting>()({
				mode_used: true,
				leaves: true,
				laya_consulted: true,
				laya_fallback: 'optional-null',
			}),
			timings_ms: k<T.QueryTimings>()({ embed: true, route: true, search: true, rerank: true, total: true }),
		},
	},
	knowledge_store: { keys: STORE_NODE, values: { kind: ['router', 'leaf'] } },
	sources: {
		paged: true,
		keys: SOURCE,
		nested: { last_run: RUN_SUMMARY, mapping: MAPPING_ROW },
		values: { type: SOURCE_TYPES, 'last_run.status': RUN_STATUSES },
	},
	source: {
		keys: SOURCE,
		nested: { last_run: RUN_SUMMARY, mapping: MAPPING_ROW },
		values: { type: SOURCE_TYPES, 'last_run.status': RUN_STATUSES },
	},
	source_runs: {
		paged: true,
		keys: SOURCE_RUN,
		nested: { replace_report: REPLACE_REPORT },
		values: { status: RUN_STATUSES },
	},
	source_run: { keys: SOURCE_RUN, values: { status: RUN_STATUSES } },
	source_test: {
		keys: k<T.ApiSample>()({
			ok: true,
			status: true,
			detail: true,
			sample: true,
			fields: true,
			truncated: true,
		}),
	},
	source_preview: {
		keys: k<T.ColumnPreview>()({ format: true, columns: true, sample_rows: true, row_count: true }),
		values: { format: ['csv', 'xlsx'] },
	},
	source_delete: {
		keys: k<T.SourceDeleted>()({ id: true, purged: true, documents_deleted: true, documents_kept: true }),
	},
	ingest_jobs: {
		paged: true,
		keys: INGEST_JOB,
		nested: { steps: INGEST_STEP },
		values: { status: INGEST_STATUSES },
	},
	ingest_job: { keys: INGEST_JOB, nested: { steps: INGEST_STEP }, values: { status: INGEST_STATUSES } },
	tools: {
		paged: true,
		keys: k<T.ToolInfo>()({
			name: true,
			description: true,
			input_schema: true,
			calls_24h: true,
			error_rate: true,
			p95_ms: true,
		}),
	},
	analytics: {
		keys: k<T.Analytics>()({ days: true, by_type: true, latency: true, previous: true }),
		nested: {
			days: { day: true, completed: true, not_eligible: true, failed: true, open: true },
			by_type: {
				request_type: true,
				cases: true,
				completed: true,
				not_eligible: true,
				failed: true,
				retries: true,
				p50_ms: true,
			},
			latency: { tool: true, p50_ms: true, p95_ms: true },
			previous: { cases: true, completed: true, not_eligible: true, failed: true },
		},
	},
	audit: {
		paged: true,
		keys: k<T.AuditRow>()({
			id: true,
			timestamp: true,
			actor_id: true,
			actor_type: true,
			action_type: true,
			resource_type: true,
			resource_id: true,
			status: true,
			details: true,
			request_id: true,
			ip_address: true,
		}),
		values: { status: ['success', 'failure'] },
	},
	conversation: {
		keys: k<T.Conversation>()({ conversation_id: true, customer_id: true, messages: true, case_ids: true }),
		nested: { messages: CHAT },
		values: { 'messages.role': ROLES },
	},
	conversation_message: {
		keys: k<T.MessageTurn>()({ turn: true, text: true, case_id: true }),
		values: { turn: ['ask', 'unclear', 'published', 'queued'] },
	},
	demo_records: {
		keys: k<T.DemoTablePage>()({
			table: true,
			primary_key: true,
			editable: true,
			columns: true,
			rows: true,
			total: true,
		}),
	},
	demo_record: { columnsOf: 'demo_records' },
	demo_scenarios: {
		keys: k<T.ScenarioList>()({ scenarios: true }),
		nested: { scenarios: k<T.Scenario>()({ name: true, description: true }) },
	},
	demo_faults: { keys: k<T.FaultState>()({ faults: true }) },
	event_bus: { keys: BUS_EVENT, nested: { envelope: ENVELOPE } },
	event_chat: { keys: CHAT, values: { role: ROLES } },
	error: { keys: k<T.ErrorBody>()({ error: true }), nested: { error: { code: true, message: true } } },
};

/**
 * Laya gates (spec 2026-10-18 §3): the key maps of src/lib/api/fixtures/gates/*.json, which
 * crates/carebus-api/tests/gates.rs shape-checks against the real handlers (`common::fixture("gates/…")`):
 * `gates` = GET /api/jobs/{job}/gates, `evaluation` = POST …/gates/evaluate, `verdicts` = GET …/gates/verdicts
 * (`{items: GateVerdict[]}`). Compile-checked against gates.ts like FIXTURES against types.ts; kept out of FIXTURES
 * because those fixtures live in a subdirectory, as fixtures/providers/ does.
 */
export const GATE_SHAPES = {
	VIEW: k<G.GatesView>()({ questions: true, provider: true }),
	PROVIDER: { bound: true, model: true } as const,
	QUESTION: k<G.GateQuestion>()({
		key: true,
		point: true,
		type: true,
		instructions: true,
		criteria: true,
		fire_when: true,
		threshold: true,
		fire_values: true,
		action: true,
		message: true,
		fail: true,
		tools: true,
		enabled: true,
		ordinal: true,
		input: 'optional',
		precheck: 'optional-null',
		updated_at: 'optional-null',
		updated_by: 'optional-null',
	}),
	EVALUATION: k<G.GateEvaluation>()({
		outcome: true,
		action: true,
		message: true,
		fired_keys: true,
		answers: true,
		latency_ms: true,
		error: 'optional-null',
		has_closed: 'optional',
		texts: 'optional',
	}),
	VERDICT: k<G.GateVerdict>()({
		id: true,
		point: true,
		conversation_id: true,
		case_id: true,
		outcome: true,
		fired_keys: true,
		action: true,
		answers: 'optional-null',
		latency_ms: true,
		error: true,
		created_at: true,
	}),
};

const EVAL_QUESTION = k<T.EvalQuestion>()({
	id: true,
	query: true,
	expected_document: true,
	expected_leaf: true,
	expected_document_ids: true,
	expected_chunk_ids: true,
	expected_text: true,
	created_by: true,
	created_at: true,
	updated_at: true,
});
const EVAL_RUN_QUESTION = k<T.EvalRunQuestion>()({
	id: true,
	query: true,
	expected_document: true,
	expected_leaf: true,
	expected_document_ids: true,
	expected_chunk_ids: true,
	expected_text: true,
});
const EVAL_REPORT = k<T.EvalModeReport>()({
	mode: true,
	n: true,
	recall_at_1: true,
	recall_at_5: true,
	recall_at_10: true,
	leaf_accuracy: true,
	laya_consulted: true,
	laya_fallbacks: true,
});
const EVAL_RESULT = k<T.EvalResult>()({
	index: true,
	question_id: true,
	mode: true,
	rank: true,
	leaf_hit: true,
});
const EVAL_RUN_HEAD = {
	id: true,
	source: true,
	root_id: true,
	modes: true,
	beam: true,
	min_score: true,
	status: true,
	error: true,
	progress: true,
	verdict: true,
	started_by: true,
	started_at: true,
	finished_at: true,
	took_ms: true,
	reports: true,
} as const;
const EVAL_RUN_KEYS = { ...EVAL_RUN_HEAD, questions: true, results: true } as const;
const EVAL_RUN = k<T.EvalRun>()(EVAL_RUN_KEYS);
const EVAL_RUN_SUMMARY = k<T.EvalRunSummary>()({ ...EVAL_RUN_HEAD, question_count: true });
const EVAL_RUN_NESTED = { questions: EVAL_RUN_QUESTION, reports: EVAL_REPORT };
const EVAL_MODES: readonly T.EvalMode[] = ['embed', 'laya', 'hybrid', 'flat'];
const EVAL_RUN_VALUES = {
	source: ['tenant', 'inline'],
	verdict: ['PASS', 'FAIL', null],
	status: ['running', 'completed', 'failed'],
	modes: EVAL_MODES,
	'reports.mode': EVAL_MODES,
};

/**
 * RAG eval (routes::knowledge_eval): the key maps of src/lib/api/fixtures/eval/*.json, which
 * crates/carebus-api/tests/knowledge_eval.rs shape-checks against the real proxy over the strata_rag fake
 * (`common::fixture("eval/…")`). Kept out of FIXTURES because those files live in a subdirectory, like gates/.
 */
export const EVAL_FIXTURES: Record<string, Spec> = {
	question: { keys: EVAL_QUESTION },
	questions: { paged: true, keys: EVAL_QUESTION },
	import: { keys: k<T.EvalImportResult>()({ added: true, total: true }) },
	runs: { paged: true, keys: EVAL_RUN_SUMMARY, nested: { reports: EVAL_REPORT }, values: EVAL_RUN_VALUES },
	run: {
		keys: EVAL_RUN,
		nested: { ...EVAL_RUN_NESTED, results: EVAL_RESULT },
		values: { ...EVAL_RUN_VALUES, 'results.mode': EVAL_MODES },
	},
	// 202: the summary of a run that is still running (reports null until it completes)
	run_started: {
		keys: EVAL_RUN_SUMMARY,
		values: { source: EVAL_RUN_VALUES.source, modes: EVAL_MODES, status: ['running'], verdict: [null] },
	},
};
