/**
 * Job-scoped admin URLs (W3 plan 14). Job pages live under `/admin/jobs/{job}/…`; the tenant pages (audit,
 * providers, users, knowledge stores and ingest jobs) stay at `/admin/…`. The pre-W3 job page URLs
 * (`/admin/cases`, `/admin`, …) redirect to the same page of the current job.
 */

/** Sub-paths of `/admin/jobs/{job}` (`''` = the job's dashboard). */
export const JOB_PAGES = [
	'',
	'/analytics',
	'/agents',
	'/buses',
	'/cases',
	'/research',
	'/tasks',
	'/matters',
	'/ontology',
	'/procedures',
	'/knowledge',
	'/tools',
	'/backends',
	'/gates',
	'/refinements',
	'/release',
] as const;

const JOB_ROUTE_ID = '/admin/jobs/[job]';

/** The admin URL of `sub` (one of JOB_PAGES, optionally followed by more path or a query) in `job`. */
export function jobPath(job: string, sub = ''): string {
	return `/admin/jobs/${encodeURIComponent(job)}${sub}`;
}

/** The job a route acts in, when it is a job page (`/admin/jobs/[job]/…`); else null. */
export function routeJob(routeId: string | null, params: Record<string, string | undefined>): string | null {
	if (!routeId || !(routeId === JOB_ROUTE_ID || routeId.startsWith(`${JOB_ROUTE_ID}/`))) return null;
	return params.job || null;
}

/**
 * Where a pre-W3 admin URL now lives in `job`, keeping its query and hash; null when `pathname` is not one of the
 * old job pages (the caller shows "page not found").
 */
export function legacyAdminTarget(
	pathname: string,
	search: string,
	hash: string,
	job: string,
): string | null {
	const path = pathname.replace(/\/+$/, '');
	if (path !== '/admin' && !path.startsWith('/admin/')) return null;
	const rest = path.slice('/admin'.length);
	if (rest === '/jobs') return null;
	const sub = JOB_PAGES.find((p) => p !== '' && (rest === p || rest.startsWith(`${p}/`)));
	if (rest !== '' && sub === undefined) return null;
	return `${jobPath(job, rest)}${search}${hash}`;
}

/** The same page in another job (`/admin/jobs/a/ontology` → `/admin/jobs/b/ontology`); null off job pages. */
export function switchJobPath(pathname: string, job: string): string | null {
	const m = /^\/admin\/jobs\/[^/]+(\/.*)?$/.exec(pathname);
	if (!m) return null;
	return jobPath(job, m[1] ?? '');
}
