import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, createClient, type Api } from '../../src/lib/api/client';
import type { IngestJob, StoreNode } from '../../src/lib/api/types';
import { app } from '../../src/lib/app.svelte';
import { deleteContinues, deleteRefusal, parentJobOf, refusalParent } from '../../src/lib/sources';
import { session } from '../../src/lib/auth/session.svelte';
import { toastState } from '../../src/lib/ui/toaster.svelte';
import JobsPage from '../../src/routes/admin/knowledge/jobs/+page.svelte';
import StoresPage from '../../src/routes/admin/knowledge/stores/+page.svelte';
import { meAs } from './me';

afterEach(() => {
	cleanup();
	vi.useRealTimers();
	app.api = null;
});

const STORE: StoreNode = {
	id: 'aaaaaaaa-0000-4000-8000-000000000002',
	parent_id: null,
	kind: 'leaf',
	name: 'returns',
	description: 'Return rules',
	token_count: 10,
	chunk_count: 2,
	children: [],
};
const job = (status: IngestJob['status']): IngestJob => ({
	id: `job-${status}`,
	store_id: STORE.id,
	source: { kind: 'url', url: 'https://help.example.nl/returns' },
	options: {},
	meta: {},
	status,
	error: null,
	document_ids: [],
	created_at: '2026-10-15T10:00:00Z',
	updated_at: '2026-10-15T10:00:00Z',
	steps: [],
});
const flush = () => new Promise((r) => setTimeout(r, 0));

describe('/admin/knowledge/jobs', () => {
	it('says when the store list is down instead of falling back to ids silently', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const stores = vi.fn(async () => {
			throw new Error('strata_rag down');
		});
		app.api = {
			stores,
			ingestJobs: async () => ({ items: [job('completed')], next: 'cursor' }),
		} as unknown as Api;
		render(JobsPage);
		expect(await screen.findByText('Stores: strata_rag down')).toBeTruthy();
		expect(screen.getByText('Showing the first 100 jobs.')).toBeTruthy();
		await fireEvent.click(screen.getByRole('button', { name: 'Try again' }));
		expect(stores).toHaveBeenCalledTimes(2);
	});

	it('polls every 5 s only while a listed job is queued or running, and stops on unmount', async () => {
		vi.useFakeTimers();
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		let status: IngestJob['status'] = 'running';
		const ingestJobs = vi.fn(async () => ({ items: [job(status)], next: null }));
		app.api = { stores: async () => ({ items: [STORE], next: null }), ingestJobs } as unknown as Api;
		const { unmount } = render(JobsPage);
		await vi.advanceTimersByTimeAsync(0);
		const first = ingestJobs.mock.calls.length;
		expect(screen.getByText(/refreshes every 5 seconds/)).toBeTruthy();
		await vi.advanceTimersByTimeAsync(5000);
		expect(ingestJobs.mock.calls.length).toBe(first + 1);
		status = 'completed';
		await vi.advanceTimersByTimeAsync(5000);
		const settled = ingestJobs.mock.calls.length;
		await vi.advanceTimersByTimeAsync(20000);
		expect(ingestJobs.mock.calls.length).toBe(settled);
		expect(screen.queryByText(/refreshes every 5 seconds/)).toBeNull();
		status = 'running';
		await fireEvent.click(screen.getByRole('button', { name: 'Refresh' }));
		await vi.advanceTimersByTimeAsync(0);
		const beforeUnmount = ingestJobs.mock.calls.length;
		unmount();
		await vi.advanceTimersByTimeAsync(20000);
		expect(ingestJobs.mock.calls.length).toBe(beforeUnmount);
	});
});

describe('/admin/knowledge/jobs cancel and delete (strata_ingest PR #11)', () => {
	const openJob = async () => {
		await fireEvent.click(await screen.findByRole('button', { name: /2026/ }));
	};
	const btn = (name: string) => screen.getByRole('button', { name }) as HTMLButtonElement;

	it('a running job cancels after a confirm and shows cancelling; it cannot be deleted', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const cancelJob = vi.fn(async () => job('cancelling'));
		const deleteJob = vi.fn();
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			ingestJobs: async () => ({ items: [job('running')], next: null }),
			ingestJob: async () => job('running'),
			cancelJob,
			deleteJob,
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		expect(btn('Delete job').disabled).toBe(true);
		expect(btn('Delete job').title).toBe('Only an ended job can be deleted: cancel it first');
		await fireEvent.click(btn('Cancel job'));
		expect(cancelJob).not.toHaveBeenCalled();
		await fireEvent.click(btn('Confirm cancel'));
		await flush();
		expect(cancelJob).toHaveBeenCalledWith('job-running');
		expect(toastState.msg).toBe('Cancelling: the step in flight finishes first.');
		expect(btn('Cancel job').disabled).toBe(true);
		expect(btn('Cancel job').title).toBe('Cancelling: the step in flight finishes first');
		expect(deleteJob).not.toHaveBeenCalled();
	});

	it('an ended job deletes after a confirm that says its documents stay', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const done = { ...job('completed'), document_ids: ['d1', 'd2'] };
		const deleteJob = vi.fn(async () => ({
			job_id: done.id,
			status: 'completed',
			deleted_jobs: 1,
			deleted_objects: 2,
			document_ids: ['d1', 'd2'],
			documents: 'retained',
		}));
		const ingestJobs = vi.fn(async () => ({ items: [done], next: null }));
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			ingestJobs,
			ingestJob: async () => done,
			deleteJob,
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		expect(btn('Cancel job').disabled).toBe(true);
		await fireEvent.click(btn('Delete job'));
		const dialog = screen.getByRole('alertdialog', { name: 'Delete job' });
		expect(dialog.textContent).toMatch(/The 2 documents it delivered\s+stay in the knowledge store/);
		const calls = ingestJobs.mock.calls.length;
		await fireEvent.click(within(dialog).getByRole('button', { name: 'Delete job' }));
		await flush();
		expect(deleteJob).toHaveBeenCalledWith(done.id);
		expect(toastState.msg).toBe('Job deleted. Its 2 documents stay in the knowledge store.');
		expect(ingestJobs.mock.calls.length).toBeGreaterThan(calls);
	});

	it('a failed delete leaves the job deleting; it then offers Retry delete and finishes', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		let status: IngestJob['status'] = 'completed';
		const deleteJob = vi
			.fn()
			.mockImplementationOnce(async () => {
				status = 'deleting';
				throw new ApiError(503, 'upstream_unavailable', 'strata_ingest is unavailable');
			})
			.mockResolvedValueOnce({
				job_id: 'job-completed',
				status: 'completed',
				deleted_jobs: 1,
				deleted_objects: 2,
				document_ids: [],
				documents: 'retained',
			});
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			// one job whose status moves (job() derives the id from the status)
			ingestJobs: async () => ({ items: [{ ...job(status), id: 'job-completed' }], next: null }),
			ingestJob: async () => ({ ...job(status), id: 'job-completed' }),
			deleteJob,
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		await fireEvent.click(btn('Delete job'));
		await fireEvent.click(
			within(screen.getByRole('alertdialog', { name: 'Delete job' })).getByRole('button', {
				name: 'Delete job',
			}),
		);
		await flush();
		await flush();
		expect(toastState.msg).toBe('Delete did not finish: strata_ingest is unavailable');
		// strata_ingest's 502 is a PARTIAL delete: some blobs are gone, the job stays `deleting`
		expect(await screen.findByText('Partly deleted — retry to finish')).toBeTruthy();
		expect(screen.queryByText('Deleting…')).toBeNull();
		expect(btn('Cancel job').disabled).toBe(true);
		expect(btn('Cancel job').title).toBe('Being deleted');
		expect(btn('Retry delete').disabled).toBe(false);
		await fireEvent.click(btn('Retry delete'));
		await fireEvent.click(
			within(screen.getByRole('alertdialog', { name: 'Delete job' })).getByRole('button', {
				name: 'Delete job',
			}),
		);
		await flush();
		expect(deleteJob).toHaveBeenCalledTimes(2);
		expect(toastState.msg).toBe('Job deleted.');
	});

	it('a 202 shows Deleting… and reads the job until it is gone, then closes it', async () => {
		vi.useFakeTimers({ shouldAdvanceTime: true });
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		let status: IngestJob['status'] = 'completed';
		const done = () => ({ ...job(status), id: 'job-x' });
		const deleteJob = vi.fn(async () => {
			status = 'deleting';
			return { job_id: 'job-x', status: 'deleting' };
		});
		const ingestJob = vi
			.fn()
			.mockImplementationOnce(async () => done()) // the drawer's read
			.mockImplementationOnce(async () => done()) // first poll: still deleting
			.mockRejectedValueOnce(new ApiError(503, 'upstream_unavailable', 'down')) // a missed poll: retried
			.mockRejectedValue(new ApiError(404, 'not_found', 'ingest job job-x not found'));
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			ingestJobs: async () => ({ items: status === 'completed' ? [done()] : [], next: null }),
			ingestJob,
			deleteJob,
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		await fireEvent.click(btn('Delete job'));
		await fireEvent.click(
			within(screen.getByRole('alertdialog', { name: 'Delete job' })).getByRole('button', {
				name: 'Delete job',
			}),
		);
		await flush();
		expect(toastState.msg).toBe('Deleting… strata_ingest finishes it in the background.');
		expect(screen.getByText('Deleting…')).toBeTruthy();
		expect(screen.queryByText('Partly deleted — retry to finish')).toBeNull();
		expect(btn('Delete job').disabled).toBe(true);
		expect(btn('Delete job').title).toBe('Being deleted in the background');
		await vi.advanceTimersByTimeAsync(2000);
		await flush();
		expect(screen.getByText('Deleting…')).toBeTruthy();
		await vi.advanceTimersByTimeAsync(2000); // the 503
		await flush();
		expect(screen.getByText('Deleting…')).toBeTruthy();
		await vi.advanceTimersByTimeAsync(2000); // 404: gone
		await flush();
		expect(toastState.msg).toBe('Job deleted.');
		expect(screen.queryByText('Deleting…')).toBeNull();
		const reads = ingestJob.mock.calls.length;
		await vi.advanceTimersByTimeAsync(30_000);
		expect(ingestJob.mock.calls.length).toBe(reads);
		vi.useRealTimers();
	});

	it('a page job offers no Delete: it opens its parent crawl with the delete confirm', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const parentId = 'aaaaaaaa-0000-4000-8000-0000000000p1';
		const page = {
			...job('completed'),
			id: 'job-page',
			source: { kind: 'url', url: 'https://help.example.nl/a', parent_job_id: parentId },
		};
		const parent = {
			...job('completed'),
			id: parentId,
			source: { kind: 'crawl', url: 'https://help.example.nl/' },
			child_job_ids: ['job-page', 'job-page-2'],
			children: { total: 2, by_status: { completed: 2 } },
		};
		const deleteJob = vi.fn();
		const ingestJob = vi.fn(async (id: string) => (id === parentId ? parent : page));
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			ingestJobs: async () => ({ items: [page], next: null }),
			ingestJob,
			deleteJob,
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		expect(screen.queryByRole('button', { name: 'Delete job' })).toBeNull();
		expect(screen.getByTestId('page-job').textContent).toContain('aaaaaaaa');
		await fireEvent.click(btn('Delete the parent crawl'));
		await flush();
		expect(ingestJob).toHaveBeenCalledWith(parentId);
		const dialog = screen.getByRole('alertdialog', { name: 'Delete job' });
		expect(dialog.textContent).toContain('with its 2 page jobs');
		expect(deleteJob).not.toHaveBeenCalled();
		expect(screen.queryByRole('button', { name: 'Delete the parent crawl' })).toBeNull();
	});

	it('a child_of_parent_job refusal says to delete the parent and opens it', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const parentId = 'aaaaaaaa-0000-4000-8000-0000000000p2';
		// a page job whose source does not name its parent: only strata_ingest's 409 does
		const page = { ...job('completed'), id: 'job-page' };
		const parent = { ...job('completed'), id: parentId, child_job_ids: ['job-page'] };
		const deleteJob = vi.fn(async () => {
			throw new ApiError(409, 'child_of_parent_job', 'page job', {
				code: 'child_of_parent_job',
				message: 'page job',
				parent_job_id: parentId,
			});
		});
		const ingestJob = vi.fn(async (id: string) => (id === parentId ? parent : page));
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			ingestJobs: async () => ({ items: [page], next: null }),
			ingestJob,
			deleteJob,
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		await fireEvent.click(btn('Delete job'));
		await fireEvent.click(
			within(screen.getByRole('alertdialog', { name: 'Delete job' })).getByRole('button', {
				name: 'Delete job',
			}),
		);
		await flush();
		expect(screen.getByTestId('delete-refusal').textContent).toContain(
			'This is a page job of crawl aaaaaaaa: it is deleted only with its parent.',
		);
		await fireEvent.click(btn('Open the parent crawl'));
		await flush();
		expect(ingestJob).toHaveBeenCalledWith(parentId);
		expect(screen.queryByTestId('delete-refusal')).toBeNull();
		expect(screen.getByRole('alertdialog', { name: 'Delete job' }).textContent).toContain(
			'with its 1 page jobs',
		);
	});

	it('another 409 keeps the generic message and offers no parent', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			ingestJobs: async () => ({ items: [job('completed')], next: null }),
			ingestJob: async () => job('completed'),
			deleteJob: async () => {
				throw new ApiError(409, 'ingest_conflict', 'job is cancelling');
			},
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		await fireEvent.click(btn('Delete job'));
		await fireEvent.click(
			within(screen.getByRole('alertdialog', { name: 'Delete job' })).getByRole('button', {
				name: 'Delete job',
			}),
		);
		await flush();
		expect(screen.getByTestId('delete-refusal').textContent).toBe(
			'Not deleted: job is cancelling (ingest_conflict)',
		);
		expect(screen.queryByRole('button', { name: 'Open the parent crawl' })).toBeNull();
	});

	it('an ended crawl parent with live page jobs can still be cancelled but not deleted', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const parent = { ...job('completed'), children: { total: 3, by_status: { completed: 1, running: 2 } } };
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			ingestJobs: async () => ({ items: [parent], next: null }),
		} as unknown as Api;
		render(JobsPage);
		await openJob();
		expect(btn('Cancel job').disabled).toBe(false);
		expect(btn('Delete job').disabled).toBe(true);
		expect(btn('Delete job').title).toBe('Its page jobs are still running: cancel it first');
	});
});

describe('/admin/knowledge/stores search preview', () => {
	it('names searched stores, sends the edited threshold and explains an empty result', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const query = vi.fn(async () => ({
			results: [],
			routing: { mode_used: 'hybrid', leaves: [STORE.id], laya_consulted: false, laya_fallback: null },
			timings_ms: { embed: 1, route: 1, search: 1, rerank: 1, total: 4 },
		}));
		app.api = { stores: async () => ({ items: [STORE], next: null }), query } as unknown as Api;
		render(StoresPage);
		await screen.findByText('returns');
		await fireEvent.input(screen.getByLabelText('Minimum score'), { target: { value: '0.5' } });
		await fireEvent.input(screen.getByLabelText('Branches (beam)'), { target: { value: '2' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Search' }));
		await flush();
		expect(query).toHaveBeenCalledWith(expect.objectContaining({ min_score: 0.5, beam: 2 }));
		expect(screen.getByText(/searched returns/)).toBeTruthy();
		expect(screen.queryByText(new RegExp(STORE.id))).toBeNull();
		expect(screen.getByText(/Only passages scoring at least 0.5 are/)).toBeTruthy();
	});

	it('names an untitled document by its source file', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		app.api = {
			stores: async () => ({ items: [STORE], next: null }),
			storeDocuments: async () => ({
				items: [
					{
						id: 'doc-uuid-1',
						store_id: STORE.id,
						title: null,
						source: 'policies/returns.md',
						token_count: 5,
						chunk_count: 1,
						content_hash: 'h',
						created_at: '2026-10-15T10:00:00Z',
					},
				],
				next: null,
			}),
		} as unknown as Api;
		render(StoresPage);
		await fireEvent.click(await screen.findByRole('button', { name: /returns/ }));
		expect(await screen.findByText('policies/returns.md')).toBeTruthy();
		expect(screen.queryByText('doc-uuid-1')).toBeNull();
	});
});

describe('deleteRefusal', () => {
	it('names the run that still follows the job, and passes any other 409 through as message and code', () => {
		const run = '7f1c9a2e-0000-4000-8000-0000000000r1';
		expect(
			deleteRefusal(new ApiError(409, 'owned_by_active_run', 'still running', { code: 'x', run_id: run })),
		).toBe('A source run (7f1c9a2e) still follows this job. It can be deleted when run 7f1c9a2e completes.');
		expect(deleteRefusal(new ApiError(409, 'owned_by_active_run', 'still running'))).toContain(
			'when that run completes',
		);
		expect(
			deleteRefusal(new ApiError(409, 'ingest_conflict', 'a crawl page job is deleted with its parent')),
		).toBe('a crawl page job is deleted with its parent (ingest_conflict)');
		expect(deleteRefusal(new ApiError(503, 'upstream_unavailable', 'down'))).toBeNull();
		const child = new ApiError(409, 'child_of_parent_job', 'm', { parent_job_id: 'bbbbbbbb-1' });
		expect(deleteRefusal(child)).toBe(
			'This is a page job of crawl bbbbbbbb: it is deleted only with its parent. Delete the parent crawl, which removes its page jobs with it.',
		);
		expect(refusalParent(child)).toBe('bbbbbbbb-1');
		expect(refusalParent(new ApiError(409, 'ingest_conflict', 'm', { parent_job_id: 'x' }))).toBeNull();
		expect(refusalParent(new ApiError(409, 'child_of_parent_job', 'm', { parent_job_id: 7 }))).toBeNull();
		expect(deleteRefusal(new Error('boom'))).toBeNull();
	});

	it('the client keeps the whole error object of a refusal', async () => {
		const body = { error: { code: 'owned_by_active_run', message: 'm', run_id: 'r-1' } };
		const fetchFn = vi.fn(
			async () =>
				new Response(JSON.stringify(body), { status: 409, headers: { 'content-type': 'application/json' } }),
		);
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		const err = await api.deleteJob('j1').catch((e: unknown) => e);
		expect(err).toBeInstanceOf(ApiError);
		expect((err as ApiError).code).toBe('owned_by_active_run');
		expect((err as ApiError).details.run_id).toBe('r-1');
	});
});

describe('page jobs and background deletes', () => {
	it('a page job names its parent in its source; a 202 is told apart from a finished delete', () => {
		expect(parentJobOf({ ...job('completed'), source: { kind: 'url', url: 'u', parent_job_id: 'p1' } })).toBe(
			'p1',
		);
		expect(parentJobOf(job('completed'))).toBeNull();
		expect(parentJobOf({ ...job('completed'), source: null })).toBeNull();
		expect(deleteContinues({ job_id: 'j', status: 'deleting' })).toBe(true);
		expect(
			deleteContinues({
				job_id: 'j',
				status: 'deleting',
				deleted_jobs: 1,
				deleted_objects: 0,
				document_ids: [],
				documents: 'retained',
			}),
		).toBe(false);
	});
});
