// RAG eval page (routes::knowledge_eval over strata_rag PR #21) and ingest job cancel/delete (strata_ingest PR #11),
// against stateful mocks of `/api/knowledge/eval/**` and `/api/ingest/jobs/**` laid over mockApi (later routes win).
import { expect, test } from '@playwright/test';
import type { Page, Route } from '@playwright/test';
import { readFileSync } from 'node:fs';
import { mockApi } from './mock-api';

const API = 'http://api.e2e.test';
const fx = (name: string) => JSON.parse(readFileSync(`src/lib/api/fixtures/${name}.json`, 'utf8'));
const ok = (route: Route, body: unknown, status = 200) =>
	route.fulfill({ status, contentType: 'application/json', body: JSON.stringify(body) });
const refuse = (route: Route, status: number, code: string, message: string) =>
	ok(route, { error: { code, message } }, status);

type Call = { method: string; path: string; contentType: string | null; body: string | null };

/**
 * Like routes::knowledge_eval over strata_rag: questions are stored; a run is started (202, `running`), scored in the
 * background (it completes on its second read, so the page sees it running first) and listed newest first.
 */
async function mockEval(page: Page, opts: { notReady?: boolean; failWith?: string } = {}): Promise<Call[]> {
	const questions: Record<string, unknown>[] = structuredClone(fx('eval/questions').items);
	const runs: Record<string, unknown>[] = [structuredClone(fx('eval/run'))];
	const calls: Call[] = [];
	/** Running runs: their finished form and how often they were read. */
	const pending = new Map<string, { done: Record<string, unknown>; reads: number }>();
	let n = 100;
	await page.route(`${API}/api/knowledge/eval/**`, async (route) => {
		const req = route.request();
		const path = new URL(req.url()).pathname.slice('/api/knowledge/eval'.length);
		const method = req.method();
		calls.push({
			method,
			path,
			contentType: (await req.headerValue('content-type')) ?? null,
			body: req.postData(),
		});
		if (path === '/questions' && method === 'GET') return ok(route, { items: questions, next: null });
		if (path === '/questions' && method === 'POST') {
			const b = req.postDataJSON();
			if (questions.some((q) => String(q.query).toLowerCase() === b.query.toLowerCase()))
				return refuse(route, 409, 'question_exists', 'a question with this query already exists');
			const row = { ...fx('eval/question'), ...b, id: `q-${n++}` };
			questions.push(row);
			return ok(route, row, 201);
		}
		if (path === '/questions/import' && method === 'POST') {
			const lines = (req.postData() ?? '').trim().split('\n').slice(1);
			for (const l of lines) {
				const [query, expected_text] = l.split(',');
				questions.push({
					...fx('eval/question'),
					id: `q-${n++}`,
					query,
					expected_text,
					expected_document: null,
				});
			}
			return ok(route, { added: lines.length, total: questions.length }, 201);
		}
		if (path.startsWith('/questions/') && method === 'DELETE') {
			const id = decodeURIComponent(path.slice('/questions/'.length));
			questions.splice(
				questions.findIndex((q) => q.id === id),
				1,
			);
			return route.fulfill({ status: 204 });
		}
		if (path === '/runs' && method === 'POST') {
			if (opts.notReady)
				return refuse(
					route,
					409,
					'eval_not_ready',
					'question `Where is my parcel?`: expected_leaf `warranty` is not a leaf store in the evaluated tree',
				);
			const b = req.postDataJSON();
			const base = fx('eval/run');
			const id = `run-${n++}`;
			const total = base.questions.length * b.modes.length;
			const done = {
				...base,
				id,
				root_id: b.root_id ?? null,
				started_at: '2026-10-20T09:00:00.000000Z',
				modes: b.modes,
				progress: { done: total, total },
				reports: b.modes.map((mode: string) => ({ ...base.reports[0], mode, recall_at_1: 1 })),
				results: base.questions.flatMap((q: { id: string }, index: number) =>
					b.modes.map((mode: string) => ({ index, question_id: q.id, mode, rank: 1, leaf_hit: null })),
				),
				...(opts.failWith
					? { status: 'failed', error: opts.failWith, verdict: null, reports: null, results: [] }
					: {}),
			};
			const running = {
				...done,
				status: 'running',
				error: null,
				progress: { done: 0, total },
				verdict: null,
				finished_at: null,
				took_ms: null,
				reports: null,
				results: [],
			};
			runs.unshift(running);
			pending.set(id, { done, reads: 0 });
			const { questions, results: _r, ...summary } = running;
			return ok(route, { ...summary, question_count: questions.length }, 202);
		}
		if (path === '/runs' && method === 'GET')
			return ok(route, {
				items: runs.map(({ results: _r, questions, ...r }) => ({
					...r,
					question_count: (questions as unknown[]).length,
				})),
				next: null,
			});
		if (path.startsWith('/runs/')) {
			const id = decodeURIComponent(path.slice('/runs/'.length));
			const p = pending.get(id);
			if (p && ++p.reads >= 2) {
				runs[runs.findIndex((r) => r.id === id)] = p.done;
				pending.delete(id);
			}
			const run = runs.find((r) => r.id === id);
			return run ? ok(route, run) : refuse(route, 404, 'not_found', 'eval run not found');
		}
		return refuse(route, 404, 'not_mocked', path);
	});
	return calls;
}

test('questions: add one, refuse a duplicate in the form, import a CSV as text/csv, delete one', async ({
	page,
}) => {
	await mockApi(page);
	const calls = await mockEval(page);
	await page.goto('/admin/knowledge/eval');
	await expect(page.getByRole('heading', { name: 'RAG eval' })).toBeVisible();
	const table = page.getByRole('table', { name: 'Eval questions' });
	await expect(table.getByRole('row')).toHaveCount(3);
	await expect(page.getByText('2 of 200')).toBeVisible();

	await page.getByRole('button', { name: 'Add question' }).first().click();
	const form = page.getByRole('form', { name: 'New question' });
	await form.getByLabel('Question').fill('where is my PARCEL?');
	await form.getByLabel('Expected text').fill('tracking');
	await form.getByRole('button', { name: 'Add question' }).click();
	await expect(form.getByRole('alert')).toHaveText('a question with this query already exists');
	await form.getByLabel('Question').fill('Can I pay later?');
	await form.getByRole('button', { name: 'Add question' }).click();
	await expect(form).toHaveCount(0);
	await expect(table.getByRole('cell', { name: 'Can I pay later?', exact: true })).toBeVisible();

	const csv = 'query,expected_text\nDo you ship abroad?,EU countries\nCan I change my order?,within 1 hour\n';
	await page.getByLabel('Import CSV').setInputFiles({
		name: 'questions.csv',
		mimeType: 'application/vnd.ms-excel',
		buffer: Buffer.from(csv),
	});
	await page.getByRole('button', { name: 'Import', exact: true }).click();
	await expect(page.getByText('Imported 2 questions; the tenant now has 5.')).toBeVisible();
	await expect(table.getByRole('cell', { name: 'Do you ship abroad?', exact: true })).toBeVisible();
	const imp = calls.find((c) => c.path === '/questions/import')!;
	expect(imp.contentType).toBe('text/csv');
	expect(imp.body).toBe(csv);

	await page.getByRole('button', { name: 'Delete Do you ship abroad?' }).click();
	await page
		.getByRole('alertdialog', { name: 'Delete question' })
		.getByRole('button', { name: 'Delete question' })
		.click();
	await expect(table.getByRole('cell', { name: 'Do you ship abroad?', exact: true })).toHaveCount(0);
	expect(calls.filter((c) => c.method === 'DELETE')).toHaveLength(1);
});

test('run an eval on a store and modes, read per-question hits, then compare it with the earlier run', async ({
	page,
}) => {
	await mockApi(page);
	const calls = await mockEval(page);
	await page.goto('/admin/knowledge/eval');
	const runs = page.getByRole('table', { name: 'Eval runs' });
	await expect(runs.getByRole('row')).toHaveCount(2);
	const root = fx('knowledge_stores').items.find((s: { kind: string }) => s.kind === 'router');
	await page.getByLabel('Store').selectOption(root.id);
	await page.getByLabel('Hybrid').uncheck();
	await page.getByLabel('Laya routing').check();
	await page.getByRole('button', { name: 'Run eval' }).click();
	// 202: the run is followed (polled every 2 s) until it completes
	await expect(page.getByTestId('run-progress')).toContainText('Running');
	await expect(page.getByRole('button', { name: 'Running…' })).toBeDisabled();
	await expect(runs.getByText('running 0/4')).toBeVisible();
	const detail = page.getByTestId('run-detail');
	await expect(detail.getByRole('table', { name: 'Recall per mode' }).getByRole('row')).toHaveCount(3);
	await expect(page.getByTestId('run-progress')).toHaveCount(0);
	await expect(page.getByRole('button', { name: 'Run eval' })).toBeEnabled();
	await expect(detail.getByRole('table', { name: 'Per-question hits' }).getByText('hit #1')).toHaveCount(4);
	const run = calls.find((c) => c.method === 'POST' && c.path === '/runs')!;
	expect(JSON.parse(run.body!)).toEqual({ root_id: root.id, modes: ['embed', 'laya'] });
	await expect(runs.getByRole('row')).toHaveCount(3);

	for (const box of await runs.getByRole('checkbox').all()) await box.check();
	await page.getByRole('button', { name: 'Compare selected' }).click();
	const cmp = page.getByTestId('run-compare');
	// only embed is in both runs: recall@1 50% → 100%, the second question moved from #2 to #1
	await expect(cmp.getByRole('table', { name: 'Recall change' }).getByRole('row')).toHaveCount(2);
	await expect(cmp.getByRole('cell', { name: '50% → 100% +50 pts' })).toBeVisible();
	await expect(cmp).toContainText('1 better, 0 worse');
	await expect(cmp.getByRole('cell', { name: '#2 → hit #1' })).toBeVisible();
});

test('a running run survives leaving the page; a failed run shows its error', async ({ page }) => {
	await mockApi(page);
	await mockEval(page, { failWith: 'laya is unavailable' });
	await page.goto('/admin/knowledge/eval');
	await page.getByRole('button', { name: 'Run eval' }).click();
	await expect(page.getByTestId('run-progress')).toBeVisible();
	await page.goto('/admin/knowledge/jobs');
	await page.goto('/admin/knowledge/eval');
	const runs = page.getByRole('table', { name: 'Eval runs' });
	await expect(runs.getByText('failed')).toBeVisible();
	await expect(page.getByText('Eval failed: laya is unavailable')).toBeVisible();
	await runs.getByRole('row').nth(1).getByRole('button', { name: 'Open' }).click();
	await expect(page.getByTestId('run-detail').getByRole('alert')).toContainText('laya is unavailable');
});

test('a run strata_rag cannot score yet explains what is missing', async ({ page }) => {
	await mockApi(page);
	await mockEval(page, { notReady: true });
	await page.goto('/admin/knowledge/eval');
	await page.getByRole('button', { name: 'Run eval' }).click();
	const alert = page.getByRole('alert');
	await expect(alert).toContainText('Not ready to evaluate');
	await expect(alert).toContainText('expected_leaf `warranty` is not a leaf store');
});

test('a tenant_user sees questions and runs but cannot change or run them', async ({ page }) => {
	await mockApi(page, { role: 'tenant_user' });
	await mockEval(page);
	await page.goto('/admin/knowledge/eval');
	await expect(page.getByRole('table', { name: 'Eval questions' }).getByRole('row')).toHaveCount(3);
	await expect(page.getByRole('button', { name: 'Add question' })).toBeDisabled();
	await expect(page.getByRole('button', { name: 'Run eval' })).toBeDisabled();
	await expect(page.getByRole('button', { name: 'Delete How long does a refund take?' })).toBeDisabled();
});

/** strata_ingest PR #11 through carebus-api: POST …/cancel (running → cancelling), DELETE of an ended job. */
async function mockJobs(
	page: Page,
	opts: { failFirstDelete?: boolean; ownedByRun?: string; background?: boolean; pageJob?: boolean } = {},
): Promise<Call[]> {
	let failDelete = !!opts.failFirstDelete;
	let owned = opts.ownedByRun;
	/** Reads left before a background (202) delete is finished by strata_ingest's sweeper. */
	let sweepAfter = 2;
	const items: Record<string, unknown>[] = structuredClone(fx('ingest_jobs').items);
	items[1]!.status = 'running';
	// the completed job is a page job of the other (strata_ingest names the parent in a page job's source)
	if (opts.pageJob) items[0]!.source = { ...(items[0]!.source as object), parent_job_id: items[1]!.id };
	const calls: Call[] = [];
	await page.route(`${API}/api/ingest/jobs**`, (route) => {
		const req = route.request();
		const path = new URL(req.url()).pathname.slice('/api/ingest/jobs'.length);
		calls.push({ method: req.method(), path, contentType: null, body: null });
		if (path === '' && req.method() === 'GET') return ok(route, { items, next: null });
		const [, id, action] = path.split('/');
		const job = items.find((j) => j.id === id);
		if (!job) return refuse(route, 404, 'not_found', 'ingest job not found');
		if (job.status === 'deleting' && opts.background && req.method() === 'GET' && --sweepAfter <= 0) {
			items.splice(items.indexOf(job), 1);
			return refuse(route, 404, 'not_found', 'ingest job not found');
		}
		if (action === 'cancel') {
			job.status = 'cancelling';
			return ok(route, job);
		}
		if (req.method() === 'DELETE') {
			if (owned) {
				const run_id = owned;
				owned = undefined;
				return ok(route, { error: { code: 'owned_by_active_run', message: 'still running', run_id } }, 409);
			}
			if (failDelete) {
				// strata_ingest's 502 (our 503): the job stays `deleting`
				failDelete = false;
				job.status = 'deleting';
				return refuse(route, 503, 'upstream_unavailable', 'strata_ingest is unavailable');
			}
			if (opts.background) {
				job.status = 'deleting';
				return ok(route, { job_id: id, status: 'deleting' }, 202);
			}
			items.splice(items.indexOf(job), 1);
			return ok(route, {
				job_id: id,
				status: job.status,
				deleted_jobs: 1,
				deleted_objects: 2,
				document_ids: job.document_ids,
				documents: 'retained',
			});
		}
		return ok(route, job);
	});
	return calls;
}

test('ingest jobs: cancel a running job (cancelling), delete an ended one whose documents stay', async ({
	page,
}) => {
	await mockApi(page);
	const calls = await mockJobs(page);
	await page.goto('/admin/knowledge/jobs');
	const rows = page.locator('table.tbl.clickable tbody tr');
	await expect(rows).toHaveCount(2);
	const drawer = page.locator('.drawer');

	await rows.filter({ hasText: 'running' }).getByRole('button').click();
	await expect(drawer.getByRole('button', { name: 'Delete job' })).toBeDisabled();
	await drawer.getByRole('button', { name: 'Cancel job' }).click();
	await drawer.getByRole('button', { name: 'Confirm cancel' }).click();
	await expect(page.getByText('Cancelling: the step in flight finishes first.')).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Cancel job' })).toBeDisabled();
	await expect(rows.filter({ hasText: 'cancelling' })).toHaveCount(1);
	expect(calls.some((c) => c.method === 'POST' && c.path.endsWith('/cancel'))).toBe(true);
	await page.keyboard.press('Escape');

	await rows.filter({ hasText: 'completed' }).getByRole('button').click();
	await expect(drawer.getByRole('button', { name: 'Cancel job' })).toBeDisabled();
	await drawer.getByRole('button', { name: 'Delete job' }).click();
	const confirm = drawer.getByRole('alertdialog', { name: 'Delete job' });
	await expect(confirm).toContainText('stay in the knowledge store');
	await confirm.getByRole('button', { name: 'Delete job' }).click();
	await expect(page.getByText(/^Job deleted\./)).toBeVisible();
	await expect(rows).toHaveCount(1);
	expect(calls.filter((c) => c.method === 'DELETE')).toHaveLength(1);
});

test('a delete storage cannot finish leaves the job deleting; Retry delete finishes it', async ({ page }) => {
	await mockApi(page);
	const calls = await mockJobs(page, { failFirstDelete: true });
	await page.goto('/admin/knowledge/jobs');
	const rows = page.locator('table.tbl.clickable tbody tr');
	const drawer = page.locator('.drawer');
	await rows.filter({ hasText: 'completed' }).getByRole('button').click();
	await drawer.getByRole('button', { name: 'Delete job' }).click();
	await drawer
		.getByRole('alertdialog', { name: 'Delete job' })
		.getByRole('button', { name: 'Delete job' })
		.click();
	await expect(drawer.getByText('Partly deleted — retry to finish')).toBeVisible();
	await expect(rows.filter({ hasText: 'deleting' })).toHaveCount(1);
	await expect(drawer.getByRole('button', { name: 'Cancel job' })).toBeDisabled();
	await drawer.getByRole('button', { name: 'Retry delete' }).click();
	await drawer
		.getByRole('alertdialog', { name: 'Delete job' })
		.getByRole('button', { name: 'Delete job' })
		.click();
	await expect(page.getByText(/^Job deleted\./)).toBeVisible();
	await expect(rows).toHaveCount(1);
	expect(calls.filter((c) => c.method === 'DELETE')).toHaveLength(2);
});

test('a job its source run still follows is not deleted until the run completes', async ({ page }) => {
	await mockApi(page);
	const calls = await mockJobs(page, { ownedByRun: '7f1c9a2e-0000-4000-8000-00000000r001' });
	await page.goto('/admin/knowledge/jobs');
	const rows = page.locator('table.tbl.clickable tbody tr');
	const drawer = page.locator('.drawer');
	await rows.filter({ hasText: 'completed' }).getByRole('button').click();
	await drawer.getByRole('button', { name: 'Delete job' }).click();
	const confirm = drawer.getByRole('alertdialog', { name: 'Delete job' });
	await confirm.getByRole('button', { name: 'Delete job' }).click();
	await expect(confirm.getByTestId('delete-refusal')).toContainText(
		'can be deleted when run 7f1c9a2e completes',
	);
	await expect(rows).toHaveCount(2);
	// the run has completed: the same confirm deletes it
	await confirm.getByRole('button', { name: 'Delete job' }).click();
	await expect(page.getByText(/^Job deleted\./)).toBeVisible();
	await expect(rows).toHaveCount(1);
	expect(calls.filter((c) => c.method === 'DELETE')).toHaveLength(2);
});

test('a delete strata_ingest finishes in the background shows Deleting… until the job is gone', async ({
	page,
}) => {
	await mockApi(page);
	const calls = await mockJobs(page, { background: true });
	await page.goto('/admin/knowledge/jobs');
	const rows = page.locator('table.tbl.clickable tbody tr');
	const drawer = page.locator('.drawer');
	await rows.filter({ hasText: 'completed' }).getByRole('button').click();
	await drawer.getByRole('button', { name: 'Delete job' }).click();
	await drawer
		.getByRole('alertdialog', { name: 'Delete job' })
		.getByRole('button', { name: 'Delete job' })
		.click();
	await expect(drawer.getByText('Deleting…')).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Delete job' })).toBeDisabled();
	await expect(page.getByText(/^Job deleted\./)).toBeVisible({ timeout: 10_000 });
	await expect(rows).toHaveCount(1);
	expect(calls.filter((c) => c.method === 'DELETE')).toHaveLength(1);
});

test('a page job offers Delete the parent crawl instead of Delete', async ({ page }) => {
	await mockApi(page);
	const calls = await mockJobs(page, { pageJob: true });
	await page.goto('/admin/knowledge/jobs');
	const rows = page.locator('table.tbl.clickable tbody tr');
	const drawer = page.locator('.drawer');
	await rows.filter({ hasText: 'completed' }).getByRole('button').click();
	await expect(drawer.getByTestId('page-job')).toBeVisible();
	await expect(drawer.getByRole('button', { name: 'Delete job' })).toHaveCount(0);
	await drawer.getByRole('button', { name: 'Delete the parent crawl' }).click();
	await expect(drawer.getByRole('alertdialog', { name: 'Delete job' })).toBeVisible();
	await expect(drawer.getByTestId('page-job')).toHaveCount(0);
	expect(calls.filter((c) => c.method === 'DELETE')).toHaveLength(0);
});
