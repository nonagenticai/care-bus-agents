import { ApiError } from './api/client';
import type {
	ChildrenSummary,
	IngestJob,
	IngestJobDeleted,
	IngestJobDeleting,
	OntologyEntity,
	ReplaceReport,
	SourceDeleted,
	RunStatus,
	Source,
	SourceInput,
	SourceMapping,
	SourceRun,
	SourceTestConfig,
	SourceTestOverride,
	SourceType,
	StoreNode,
	UnsavedSourceTest,
} from './api/types';

export const SOURCE_TYPES: [SourceType, string, string][] = [
	['website', 'Website', 'Pages fetched from public URLs'],
	['api', 'API', 'Records from a REST endpoint, mapped to the ontology'],
	['file', 'File', 'Documents you upload with each re-sync'],
	['datatable', 'Datatable', 'A CSV or XLSX table, mapped to the ontology'],
];

/** How a website source finds its pages (`config.mode`; a config without one is `urls`). */
export type WebsiteMode = 'urls' | 'crawl' | 'sitemap';
export const WEBSITE_MODES: [WebsiteMode, string, string][] = [
	['urls', 'Listed pages', 'Fetch the URLs you list, one per line'],
	['crawl', 'Crawl', 'Start at one page and follow its links'],
	['sitemap', 'Sitemap', 'Read the pages a sitemap.xml lists'],
];

/** api and datatable carry a mapping and credentials (strata_ingest's extended contract). */
export const structured = (t: SourceType) => t === 'api' || t === 'datatable';

export interface SourceDraft {
	name: string;
	type: SourceType;
	urls: string;
	/** Website: `urls` lists pages; `crawl` / `sitemap` start from the single `url`. */
	mode: WebsiteMode;
	/** The endpoint or table URL (api, datatable), or a crawl's start page / the sitemap URL (website). */
	url: string;
	/** Crawl settings; null / empty = strata_ingest's default (depth 2, 50 pages, any domain, no query). */
	maxDepth: number | null;
	maxPages: number | null;
	/** One host per line. */
	allowedDomains: string;
	linkQuery: string;
	/** Sitemap settings; null / empty = strata_ingest's default (200 URLs, no filters). */
	maxUrls: number | null;
	/** URL fragments, one per line. */
	include: string;
	exclude: string;
	authType: 'none' | 'bearer' | 'header';
	headerName: string;
	format: 'csv' | 'xlsx';
	sheet: string;
	/** Datatable: the first row holds the column names (strata_ingest's `header_row`, default true). */
	headerRow: boolean;
	targetStoreId: string;
	schedule: string;
	enabled: boolean;
	mapping: SourceMapping[];
	options: Record<string, unknown> | null;
	/** Typed in this session only; credentials are write-only and never prefilled. */
	secret: string;
	clearSecret: boolean;
}

const str = (v: unknown) => (typeof v === 'string' ? v : '');
const num = (v: unknown) => (typeof v === 'number' ? v : null);
const lines = (v: unknown) => (Array.isArray(v) ? v.map(str).join('\n') : '');
const listOf = (t: string) =>
	t
		.split('\n')
		.map((u) => u.trim())
		.filter(Boolean);
const obj = (v: unknown) => (v && typeof v === 'object' ? (v as Record<string, unknown>) : {});
const modeOf = (v: unknown): WebsiteMode => (v === 'crawl' || v === 'sitemap' ? v : 'urls');

export function draftOf(s: Source | null): SourceDraft {
	const c = (s?.config ?? {}) as Record<string, unknown>;
	const auth = (c.auth ?? {}) as Record<string, unknown>;
	const crawl = obj(c.crawl);
	const sitemap = obj(c.sitemap);
	return {
		name: s?.name ?? '',
		type: s?.type ?? 'website',
		urls: lines(c.urls),
		mode: modeOf(c.mode),
		url: str(c.url),
		maxDepth: num(crawl.max_depth),
		maxPages: num(crawl.max_pages),
		allowedDomains: lines(crawl.allowed_domains),
		linkQuery: str(crawl.link_query),
		maxUrls: num(sitemap.max_urls),
		include: lines(sitemap.include),
		exclude: lines(sitemap.exclude),
		authType: auth.type === 'bearer' || auth.type === 'header' ? auth.type : 'none',
		headerName: str(auth.header_name),
		format: c.format === 'xlsx' ? 'xlsx' : 'csv',
		sheet: str(c.sheet),
		headerRow: c.header_row !== false,
		targetStoreId: s?.target_store_id ?? '',
		schedule: s?.schedule ?? '',
		enabled: s?.enabled ?? true,
		mapping: (s?.mapping ?? []).map((m) => ({ ...m })),
		options: c.options && typeof c.options === 'object' ? (c.options as Record<string, unknown>) : null,
		secret: '',
		clearSecret: false,
	};
}

/**
 * The POST/PUT body. `credentials` is left out (= keep) unless a new secret was typed, removal was asked for,
 * or the type no longer uses credentials while some are stored (= clear).
 */
export function inputOf(d: SourceDraft, hadCredentials: boolean): SourceInput {
	const config: Record<string, unknown> = {};
	if (d.type === 'website') Object.assign(config, websiteConfig(d));
	if (structured(d.type)) {
		config.url = d.url.trim();
		config.auth =
			d.authType === 'header' ? { type: 'header', header_name: d.headerName.trim() } : { type: d.authType };
	}
	if (d.type === 'datatable') {
		config.format = d.format;
		if (d.sheet.trim()) config.sheet = d.sheet.trim();
		if (!d.headerRow) config.header_row = false;
	}
	if (d.options) config.options = d.options;
	const input: SourceInput = {
		name: d.name.trim(),
		type: d.type,
		config,
		mapping: structured(d.type) ? d.mapping.filter((m) => m.source_field.trim() !== '') : [],
		target_store_id: d.targetStoreId,
		schedule: d.schedule.trim() === '' ? null : d.schedule.trim(),
		enabled: d.enabled,
	};
	const uses = structured(d.type) && d.authType !== 'none';
	if (uses && d.secret !== '') input.credentials = { secret: d.secret };
	else if (hadCredentials && (d.clearSecret || !uses)) input.credentials = null;
	return input;
}

/**
 * A website's config without `options`. `urls` mode is the plain `{urls}` (no `mode` key, as before crawl
 * existed); `crawl` / `sitemap` carry the one start `url` and only the settings that were filled in.
 */
function websiteConfig(d: SourceDraft): Record<string, unknown> {
	if (d.mode === 'urls') return { urls: listOf(d.urls) };
	const set: Record<string, unknown> = {};
	const put = (k: string, v: unknown) => {
		if (v != null && v !== '' && !(Array.isArray(v) && v.length === 0)) set[k] = v;
	};
	if (d.mode === 'crawl') {
		put('max_depth', d.maxDepth);
		put('max_pages', d.maxPages);
		put('allowed_domains', listOf(d.allowedDomains));
		put('link_query', d.linkQuery.trim());
	} else {
		put('max_urls', d.maxUrls);
		put('include', listOf(d.include));
		put('exclude', listOf(d.exclude));
	}
	const config: Record<string, unknown> = { mode: d.mode, url: d.url.trim() };
	if (Object.keys(set).length) config[d.mode] = set;
	return config;
}

const between = (n: number | null, lo: number, hi: number) =>
	n == null || (Number.isInteger(n) && n >= lo && n <= hi);
/** Mirrors carebus-api sources.rs `host_ok`: a bare host name, no scheme, port or path. */
export const hostOk = (h: string) =>
	h.length <= 253 && h.split('.').every((l) => /^[A-Za-z0-9]([A-Za-z0-9-]{0,61}[A-Za-z0-9])?$/.test(l));

/** The website-only problems (mirrors carebus-api sources.rs `validate_website`). */
function websiteErrors(d: SourceDraft): string[] {
	const e: string[] = [];
	if (d.mode === 'urls') {
		const n = listOf(d.urls).length;
		if (n === 0) e.push('Add at least one URL, one per line.');
		if (n > 50) e.push('A website source takes at most 50 URLs.');
		return e;
	}
	if (!/^https?:\/\/\S+$/.test(d.url.trim()))
		e.push(
			d.mode === 'crawl'
				? 'The start page must start with http:// or https://.'
				: 'The sitemap URL must start with http:// or https://.',
		);
	const tooMany = (t: string) => listOf(t).length > 50;
	if (d.mode === 'crawl') {
		if (!between(d.maxDepth, 1, 5)) e.push('Link depth is a whole number from 1 to 5.');
		if (!between(d.maxPages, 1, 200)) e.push('Max pages is a whole number from 1 to 200.');
		const hosts = listOf(d.allowedDomains);
		if (tooMany(d.allowedDomains) || !hosts.every(hostOk))
			e.push('Allowed domains are up to 50 host names, one per line, such as help.example.nl.');
		if (d.linkQuery.trim().length > 500) e.push('The link query is at most 500 characters.');
	} else {
		if (!between(d.maxUrls, 1, 1000)) e.push('Max URLs is a whole number from 1 to 1000.');
		for (const [label, t] of [
			['Include', d.include],
			['Exclude', d.exclude],
		] as const)
			if (tooMany(t) || listOf(t).some((x) => x.length > 200))
				e.push(`${label} takes up to 50 URL fragments of at most 200 characters.`);
	}
	return e;
}

/** Mirrors carebus-api sources.rs `header_name_ok`: RFC 7230 token, 1-64 chars, minus headers the proxy must own. */
const FORBIDDEN_HEADERS = [
	'host',
	'content-length',
	'transfer-encoding',
	'connection',
	'upgrade',
	'te',
	'trailer',
	'cookie',
];
export const headerNameOk = (h: string) =>
	h.length >= 1 &&
	h.length <= 64 &&
	/^[A-Za-z0-9!#$%&'*+\-.^_`|~]+$/.test(h) &&
	!FORBIDDEN_HEADERS.includes(h.toLowerCase());

export function deleteMessage(r: SourceDeleted): string {
	const n = (c: number) => `${c} document${c === 1 ? '' : 's'}`;
	return r.purged
		? `Source deleted with its ${n(r.documents_deleted)}.`
		: `Source deleted. ${n(r.documents_kept)} kept in strata_rag.`;
}

export function draftErrors(d: SourceDraft): string[] {
	const e: string[] = [];
	if (!d.name.trim()) e.push('Give the source a name.');
	if (!d.targetStoreId) e.push(STORE_MISSING);
	if (d.type === 'website') e.push(...websiteErrors(d));
	if (structured(d.type) && !/^https?:\/\/\S+$/.test(d.url.trim()))
		e.push('The URL must start with http:// or https://.');
	if (structured(d.type) && d.authType === 'header' && !headerNameOk(d.headerName.trim()))
		e.push('Name a valid header, for example X-Api-Key (not Host or Cookie).');
	if (d.schedule.trim() && d.schedule.trim().split(/\s+/).length !== 5)
		e.push('A schedule has five cron fields, for example 0 3 * * *.');
	const fields = d.mapping.map((m) => m.source_field.trim()).filter(Boolean);
	if (new Set(fields).size !== fields.length) e.push('Each source field can be mapped once.');
	if (d.mapping.some((m) => m.source_field.trim() && !m.entity))
		e.push('Pick an ontology attribute for every mapped field.');
	return e;
}

export function attributeOptions(
	entities: OntologyEntity[],
): { entity: string; attribute: string; label: string }[] {
	return entities
		.filter((e) => !e.hidden)
		.flatMap((e) =>
			e.attributes.map((a) => ({
				entity: e.name,
				attribute: a.name,
				label: `${e.name}.${a.name}${a.pii ? ' (PII)' : ''}`,
			})),
		);
}

/** Depth-first from the roots, children in strata_rag's order; an orphan or unreachable node is shown at depth 0. */
export function storeTree(stores: StoreNode[]): { node: StoreNode; depth: number }[] {
	const byId = new Map(stores.map((s) => [s.id, s]));
	const out: { node: StoreNode; depth: number }[] = [];
	const seen = new Set<string>();
	const visit = (n: StoreNode, depth: number) => {
		if (seen.has(n.id)) return;
		seen.add(n.id);
		out.push({ node: n, depth });
		for (const c of n.children) {
			const child = byId.get(c);
			if (child) visit(child, depth + 1);
		}
	};
	for (const s of stores) if (!s.parent_id || !byId.has(s.parent_id)) visit(s, 0);
	for (const s of stores) visit(s, 0);
	return out;
}

export const leafStores = (stores: StoreNode[]) => stores.filter((s) => s.kind === 'leaf');

/** `cost_micros` (millionths of a euro) as euros. `null` is unknown, shown as "—", never "€0". */
export function euros(micros: number | null | undefined): string {
	if (micros == null) return '—';
	const e = micros / 1_000_000;
	return '€' + (e === 0 ? '0.00' : e < 0.01 ? e.toFixed(4) : e.toFixed(2));
}

export function jobCost(job: IngestJob): number | null {
	const known = job.steps.map((s) => s.cost_micros).filter((c): c is number => c != null);
	return known.length ? known.reduce((a, b) => a + b, 0) : null;
}

export function jobSourceLabel(job: IngestJob): string {
	const s = (job.source ?? {}) as Record<string, unknown>;
	if (typeof s.url === 'string') return s.url;
	// crawl / sitemap: `{kind, config: {url, …}}`
	const c = obj(s.config);
	if (typeof c.url === 'string') return `${String(s.kind ?? '')} ${c.url}`.trim();
	if (typeof s.filename === 'string') return s.filename;
	return typeof s.kind === 'string' ? s.kind : '—';
}

export const RUN_LABELS: Record<RunStatus, string> = {
	running: 'Running',
	completed: 'Completed',
	failed: 'Failed',
	unsupported: 'Not supported by strata_ingest yet',
};

/** The toast after a re-sync, from the run the server returned: a refused request must not read as "started". */
export function resyncToast(r: Pick<SourceRun, 'status' | 'error'>): string {
	switch (r.status) {
		case 'running':
			return 'Re-sync started.';
		case 'completed':
			return 'Re-sync completed.';
		case 'unsupported':
			return r.error ? `${RUN_LABELS.unsupported}: ${r.error}` : `${RUN_LABELS.unsupported}.`;
		default:
			return r.error ? `Re-sync failed: ${r.error}` : 'Re-sync failed.';
	}
}

/** A crawl or sitemap job's page jobs in one line: "12 pages: 10 completed · 2 failed". */
export function childrenSummary(c: ChildrenSummary | null | undefined): string {
	if (!c) return '';
	const parts = Object.entries(c.by_status)
		.filter(([, n]) => n > 0)
		.map(([st, n]) => `${n} ${st}`);
	const head = `${c.total} page${c.total === 1 ? '' : 's'}`;
	return parts.length ? `${head}: ${parts.join(' · ')}` : head;
}

export function replaceSummary(r: ReplaceReport | null): string {
	if (!r) return '';
	const parts = [`Replaced ${r.deleted} older document${r.deleted === 1 ? '' : 's'}`];
	if (r.kept_shared) parts.push(`${r.kept_shared} unchanged kept`);
	if (r.already_gone) parts.push(`${r.already_gone} already gone`);
	if (r.failed.length)
		parts.push(`${r.failed.length} could not be deleted (retried on the next completed run)`);
	return parts.join(' · ');
}

export const STORE_MISSING = 'Pick the store the documents go into.';

export type TestPlan =
	/** An unsaved draft: POST /api/sources/test with the connection settings only; nothing is stored. */
	| { kind: 'unsaved'; body: UnsavedSourceTest; note: string }
	/** A saved source: POST /api/sources/{id}/test, with the edited settings when the form differs (`body`). */
	| { kind: 'saved'; id: string; body: SourceTestOverride | null; note: string }
	| { kind: 'blocked'; reasons: string[] };

/** The connection part of a draft: what a test sends (`config.url` + `config.auth`). */
export function connectionOf(d: SourceDraft): SourceTestConfig {
	return {
		url: d.url.trim(),
		auth:
			d.authType === 'header' ? { type: 'header', header_name: d.headerName.trim() } : { type: d.authType },
	};
}

const originOf = (u: string) => {
	try {
		return new URL(u).origin;
	} catch {
		return null;
	}
};

/** What blocks a test: only the connection settings matter (no name or target store is needed). */
export function connectionErrors(d: SourceDraft, opts: { secretInSettings?: boolean } = {}): string[] {
	const e: string[] = [];
	if (!/^https?:\/\/\S+$/.test(d.url.trim())) e.push('The URL must start with http:// or https://.');
	if (d.authType === 'header' && !headerNameOk(d.headerName.trim()))
		e.push('Name a valid header, for example X-Api-Key (not Host or Cookie).');
	if (opts.secretInSettings) e.push('Move the key out of the URL or header name into Credentials.');
	return e;
}

/**
 * What "Test connection" sends for this api draft. Nothing is saved: an unsaved draft goes to the stateless
 * endpoint; a saved source is tested as stored, or with the form's edits as an override. A secret typed in the form
 * is sent for that request only; without one, the server uses the stored secret, but only against the saved host.
 */
export function testPlan(
	d: SourceDraft,
	saved: Source | null,
	opts: { secretInSettings?: boolean } = {},
): TestPlan {
	const errs = connectionErrors(d, opts);
	if (errs.length) return { kind: 'blocked', reasons: errs };
	const config = connectionOf(d);
	const typed = d.authType !== 'none' && d.secret !== '' ? { secret: d.secret } : undefined;
	if (!saved)
		return {
			kind: 'unsaved',
			body: { type: 'api', config, ...(typed ? { credentials: typed } : {}) },
			note: 'Tests the settings above. Nothing is saved.',
		};
	const body: SourceTestOverride = {};
	const stored = connectionOf(draftOf(saved));
	if (JSON.stringify(config) !== JSON.stringify(stored)) body.config = config;
	if (typed) body.credentials = typed;
	else if (d.clearSecret) body.credentials = null;
	const keepsStored = saved.has_credentials && d.authType !== 'none' && body.credentials === undefined;
	if (keepsStored && body.config && originOf(config.url) !== originOf(stored.url))
		return {
			kind: 'blocked',
			reasons: [
				'Enter the secret again to test a different host; the stored one is only sent to the saved host.',
			],
		};
	const edited = Object.keys(body).length > 0;
	return {
		kind: 'saved',
		id: saved.id,
		body: edited ? body : null,
		note: edited ? 'Tests your edits without saving them.' : '',
	};
}

/** True when saving the draft would send something different from what is stored. */
export function draftDirty(d: SourceDraft, saved: Source): boolean {
	const had = saved.has_credentials;
	return JSON.stringify(inputOf(d, had)) !== JSON.stringify(inputOf(draftOf(saved), had));
}

/** A failed test request as one line for the inline error (403/0 are the role and reachability cases). */
export function testErrorText(e: unknown): string {
	const x = (e ?? {}) as { status?: number; code?: string; message?: string };
	if (x.status === 403) return 'You need the tenant_admin role to test a source.';
	if (x.status === 0) return 'carebus-api is unreachable. Nothing was tested.';
	if (x.code === 'address_refused')
		return `The address is refused (private, loopback or internal hosts are blocked): ${x.message ?? ''}`.trim();
	return x.message || 'The test failed.';
}

/**
 * Rows for every discovered field not yet mapped. A field's ontology attribute is filled when exactly one visible
 * attribute has the same name (case-insensitive); with several, the one on an entity this mapping already uses wins;
 * otherwise it is left for the user to pick.
 */
export function mapAllFields(
	fields: string[],
	rows: SourceMapping[],
	options: { entity: string; attribute: string }[],
): { rows: SourceMapping[]; added: number; matched: number } {
	const mapped = new Set(rows.map((r) => r.source_field.trim().toLowerCase()).filter(Boolean));
	const used = new Set(rows.map((r) => r.entity).filter(Boolean));
	const out = [...rows];
	let added = 0;
	let matched = 0;
	for (const f of fields) {
		const key = f.trim().toLowerCase();
		if (!key || mapped.has(key)) continue;
		mapped.add(key);
		const hits = options.filter((o) => o.attribute.toLowerCase() === key);
		const pick = hits.length === 1 ? hits[0] : hits.find((o) => used.has(o.entity));
		out.push({ source_field: f, entity: pick?.entity ?? '', attribute: pick?.attribute ?? '' });
		added++;
		if (pick) matched++;
	}
	return { rows: out, added, matched };
}

/** How many of `fields` are not mapped yet (the N in "Map all N fields"). */
export const unmappedCount = (fields: string[], rows: SourceMapping[]) => {
	const mapped = new Set(rows.map((r) => r.source_field.trim().toLowerCase()));
	return new Set(fields.map((f) => f.trim().toLowerCase()).filter((f) => f && !mapped.has(f))).size;
};

/**
 * Why carebus-api refused to delete an ingest job (409), in words for the delete confirm, or null for any other
 * error. `owned_by_active_run`: a source run still follows the job; `child_of_parent_job`: a crawl/sitemap page job,
 * deleted only with its parent (`refusalParent` names it); any other 409 is shown as its message and code.
 */
export function deleteRefusal(e: unknown): string | null {
	if (!(e instanceof ApiError) || e.status !== 409) return null;
	if (e.code === 'owned_by_active_run') {
		const run = typeof e.details.run_id === 'string' ? e.details.run_id : null;
		return run
			? `A source run (${run.slice(0, 8)}) still follows this job. It can be deleted when run ${run.slice(0, 8)} completes.`
			: 'A source run still follows this job. It can be deleted when that run completes.';
	}
	if (e.code === 'child_of_parent_job') {
		const parent = refusalParent(e);
		return `This is a page job of crawl ${parent ? parent.slice(0, 8) : 'job'}: it is deleted only with its parent. Delete the parent crawl, which removes its page jobs with it.`;
	}
	return `${e.message} (${e.code})`;
}

/** The parent job a `child_of_parent_job` refusal names, or null. */
export function refusalParent(e: unknown): string | null {
	if (!(e instanceof ApiError) || e.code !== 'child_of_parent_job') return null;
	return typeof e.details.parent_job_id === 'string' ? e.details.parent_job_id : null;
}

/** A crawl/sitemap page job's parent (strata_ingest puts `parent_job_id` in a page job's source), or null. */
export function parentJobOf(j: IngestJob): string | null {
	const src = j.source as { parent_job_id?: unknown } | null;
	return src && typeof src === 'object' && typeof src.parent_job_id === 'string' ? src.parent_job_id : null;
}

/** A 202 to a job delete: strata_ingest goes on deleting it in the background. */
export const deleteContinues = (r: IngestJobDeleted | IngestJobDeleting): r is IngestJobDeleting =>
	!('deleted_jobs' in r);
