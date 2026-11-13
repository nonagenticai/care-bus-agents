import { readFileSync } from 'node:fs';
import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, type Api } from '../../src/lib/api/client';
import type { EvalQuestion, EvalRun, EvalRunSummary, Paged } from '../../src/lib/api/types';
import { app } from '../../src/lib/app.svelte';
import { session } from '../../src/lib/auth/session.svelte';
import {
	change,
	compare,
	delta,
	emptyForm,
	expectations,
	formOf,
	formProblem,
	grid,
	inputOf,
	parseIds,
	progressLabel,
	rankLabel,
	runChip,
	withRun,
} from '../../src/lib/eval';
import { afterPollError, POLL_MAX_FAILURES } from '../../src/lib/poll';
import { toastState } from '../../src/lib/ui/toaster.svelte';
import EvalPage from '../../src/routes/admin/knowledge/eval/+page.svelte';
import { meAs } from './me';

const fx = <T>(name: string): T => JSON.parse(readFileSync(`src/lib/api/fixtures/eval/${name}.json`, 'utf8'));
const flush = () => new Promise((r) => setTimeout(r, 0));

afterEach(() => {
	cleanup();
	app.api = null;
});

describe('eval logic', () => {
	it('parses ids from any separator, once each', () => {
		expect(parseIds(' a;b, c\nd |a ')).toEqual(['a', 'b', 'c', 'd']);
		expect(parseIds('  ')).toEqual([]);
	});

	it('a blank optional field is null (an edit clears it) and an expectation is required', () => {
		const f = { ...emptyForm(), query: '  Where is my parcel? ', expected_leaf: ' ' };
		expect(formProblem(f)).toMatch(/at least one expectation/);
		expect(formProblem({ ...f, query: '' })).toBe('Write the question.');
		const ok = { ...f, expected_chunk_ids: 'c1\nc2' };
		expect(formProblem(ok)).toBeNull();
		expect(inputOf(ok)).toEqual({
			query: 'Where is my parcel?',
			expected_document: null,
			expected_leaf: null,
			expected_text: null,
			expected_document_ids: [],
			expected_chunk_ids: ['c1', 'c2'],
		});
		const q = fx<EvalQuestion>('question');
		expect(inputOf(formOf(q))).toEqual({
			query: q.query,
			expected_document: q.expected_document,
			expected_leaf: q.expected_leaf,
			expected_text: q.expected_text,
			expected_document_ids: q.expected_document_ids,
			expected_chunk_ids: q.expected_chunk_ids,
		});
		expect(expectations(q)).toEqual([
			'document “Returns policy”',
			'text “within 5 working days”',
			'1 document id',
			'1 chunk id',
		]);
	});

	it('lays a run out per question and mode', () => {
		const rows = grid(fx<EvalRun>('run'));
		expect(rows.map((r) => r.question.query)).toEqual([
			'How long does a refund take?',
			'Where is my parcel?',
		]);
		expect(rows[1]!.cells.hybrid?.rank).toBe(2);
		expect(rows[0]!.cells.embed?.leaf_hit).toBe(true);
		expect(rankLabel(1)).toBe('hit #1');
		expect(rankLabel(3)).toBe('#3');
		expect(rankLabel(null)).toBe('miss');
	});

	it('a miss is worse than any hit; a lower rank is better', () => {
		expect(change(null, 4)).toBe('better');
		expect(change(2, null)).toBe('worse');
		expect(change(3, 1)).toBe('better');
		expect(change(null, null)).toBe('same');
		expect(delta(0.125)).toBe('+13 pts');
		expect(delta(-0.5)).toBe('−50 pts');
		expect(delta(0.001)).toBe('±0');
	});

	it('labels a run by its state and keeps a listed run a summary when it is re-read', () => {
		const started = fx<EvalRunSummary>('run_started');
		expect(runChip(started)).toEqual({ text: 'running 0/4', variant: 'work' });
		expect(runChip({ ...started, progress: undefined })).toEqual({ text: 'running', variant: 'work' });
		expect(runChip({ ...started, status: 'failed' })).toEqual({ text: 'failed', variant: 'err' });
		expect(runChip({ ...started, status: 'completed', verdict: 'PASS' })).toEqual({
			text: 'PASS',
			variant: 'ok',
		});
		expect(runChip({ ...started, status: 'completed', verdict: 'FAIL' }).variant).toBe('err');
		expect(progressLabel({ done: 12, total: 40 })).toBe('12 / 40 searches');
		expect(progressLabel(null)).toBe('');
		const read = fx<EvalRun>('run');
		const merged = withRun(started, read);
		expect(merged.status).toBe('completed');
		expect(merged.question_count).toBe(read.questions.length);
		expect(['questions', 'results'].filter((k) => k in merged)).toEqual([]);
	});

	it('compares two runs on their shared modes, matching questions by id', () => {
		const a = fx<EvalRun>('run');
		const b: EvalRun = structuredClone(a);
		b.id = 'run-b';
		b.modes = ['embed', 'flat'];
		b.reports = [
			{ ...a.reports![0]!, recall_at_1: 1, recall_at_5: 1, recall_at_10: 1 },
			{ ...a.reports![0]!, mode: 'flat', recall_at_1: 0, recall_at_5: 0, recall_at_10: 0 },
		];
		// question 2 now ranks first in embed; question 1 drops out of the top 10; a new inline question appears
		b.questions[0]!.query = 'How long does a refund take (edited)?';
		b.questions.push({ ...b.questions[1]!, id: null, query: 'Brand new' });
		b.results = [
			{ index: 0, question_id: a.questions[0]!.id, mode: 'embed', rank: null, leaf_hit: false },
			{ index: 1, question_id: a.questions[1]!.id, mode: 'embed', rank: 1, leaf_hit: null },
			{ index: 2, question_id: null, mode: 'embed', rank: 1, leaf_hit: null },
		];
		const c = compare(a, b);
		expect(c.modes).toEqual(['embed']);
		expect(c.recall.map((r) => [r.mode, delta(r.d1)])).toEqual([['embed', '+50 pts']]);
		expect(c.rows[0]).toEqual({
			query: 'How long does a refund take (edited)?',
			cells: { embed: { a: 1, b: null, change: 'worse' } },
		});
		expect(c.rows[1]!.cells.embed).toEqual({ a: 2, b: 1, change: 'better' });
		expect(c.rows[2]).toEqual({ query: 'Brand new', cells: {}, only: 'b' });
		expect([c.better, c.worse]).toEqual([1, 1]);
	});
});

function fakeApi(over: Partial<Api> = {}) {
	const questions = fx<Paged<EvalQuestion>>('questions');
	const runs = fx<Paged<EvalRunSummary>>('runs');
	const api = {
		evalQuestions: vi.fn(async () => questions),
		stores: vi.fn(async () => ({
			items: [
				{
					id: '7f1c9a2e-0000-4000-8000-0000000000a0',
					parent_id: null,
					kind: 'router',
					name: 'care',
					description: '',
					token_count: 0,
					chunk_count: 0,
					children: [],
				},
			],
			next: null,
		})),
		evalRuns: vi.fn(async () => runs),
		evalRun: vi.fn(async () => fx<EvalRun>('run')),
		createEvalQuestion: vi.fn(async () => questions.items[0]!),
		updateEvalQuestion: vi.fn(async () => questions.items[0]!),
		deleteEvalQuestion: vi.fn(async () => undefined),
		importEvalQuestions: vi.fn(async () => ({ added: 2, total: 4 })),
		runEval: vi.fn(async () => fx<EvalRunSummary>('run_started')),
		...over,
	};
	app.api = api as unknown as Api;
	return api;
}

describe('/admin/knowledge/eval', () => {
	it('lists questions with what counts as a hit, and runs with recall per mode', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const api = fakeApi();
		render(EvalPage);
		const table = await screen.findByRole('table', { name: 'Eval questions' });
		expect(within(table).getByText('How long does a refund take?')).toBeTruthy();
		expect(within(table).getByText(/document “Returns policy” or text “within 5 working days”/)).toBeTruthy();
		expect(screen.getByText('2 of 200')).toBeTruthy();
		const runs = await screen.findByRole('table', { name: 'Eval runs' });
		expect(within(runs).getByText('care')).toBeTruthy();
		expect(
			within(runs)
				.getAllByText(/50% \/ 100% \/ 100%/)
				.map((e) => e.textContent?.trim()),
		).toEqual(['embed 50% / 100% / 100%', 'hybrid 50% / 100% / 100%']);
		// the listed summary has no questions: Open reads the full run for its per-question hits
		await fireEvent.click(within(runs).getByRole('button', { name: 'Open' }));
		const hits = await screen.findByRole('table', { name: 'Per-question hits' });
		expect(api.evalRun).toHaveBeenCalledWith('7f1c9a2e-0000-4000-8000-000000000e03');
		expect(within(hits).getByText('Where is my parcel?')).toBeTruthy();
	});

	it('adds a question from the form; a strata_rag refusal stays in the form', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const createEvalQuestion = vi
			.fn()
			.mockRejectedValueOnce(
				new ApiError(409, 'question_exists', 'a question with this query already exists'),
			)
			.mockResolvedValueOnce(fx('question'));
		fakeApi({ createEvalQuestion } as Partial<Api>);
		render(EvalPage);
		await screen.findByRole('table', { name: 'Eval questions' });
		await fireEvent.click(screen.getAllByRole('button', { name: 'Add question' })[0]!);
		const save = () =>
			within(screen.getByRole('form', { name: 'New question' })).getByRole('button', {
				name: 'Add question',
			});
		expect((save() as HTMLButtonElement).disabled).toBe(true);
		await fireEvent.input(screen.getByLabelText('Question'), { target: { value: 'Can I pay later?' } });
		await fireEvent.input(screen.getByLabelText('Expected text'), { target: { value: 'Klarna' } });
		await fireEvent.click(save());
		await flush();
		expect(createEvalQuestion).toHaveBeenCalledWith({
			query: 'Can I pay later?',
			expected_document: null,
			expected_leaf: null,
			expected_text: 'Klarna',
			expected_document_ids: [],
			expected_chunk_ids: [],
		});
		expect(screen.getByRole('alert').textContent).toContain('already exists');
		await fireEvent.click(save());
		await flush();
		expect(screen.queryByRole('form', { name: 'New question' })).toBeNull();
		expect(toastState.msg).toBe('Question added.');
	});

	it('edits with null for a cleared field and deletes after a confirm', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const api = fakeApi();
		render(EvalPage);
		await fireEvent.click(await screen.findByRole('button', { name: 'Edit How long does a refund take?' }));
		await fireEvent.input(screen.getByLabelText('Expected leaf store'), { target: { value: '' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Save question' }));
		await flush();
		expect(api.updateEvalQuestion).toHaveBeenCalledWith(
			'7f1c9a2e-0000-4000-8000-000000000e01',
			expect.objectContaining({ expected_leaf: null, expected_document: 'Returns policy' }),
		);
		await fireEvent.click(screen.getByRole('button', { name: 'Delete Where is my parcel?' }));
		expect(api.deleteEvalQuestion).not.toHaveBeenCalled();
		await fireEvent.click(screen.getByRole('button', { name: 'Delete question' }));
		await flush();
		expect(api.deleteEvalQuestion).toHaveBeenCalledWith('7f1c9a2e-0000-4000-8000-000000000e02');
	});

	it('runs with the picked store and modes, shows the run, and explains eval_not_ready', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const runEval = vi
			.fn()
			.mockRejectedValueOnce(
				new ApiError(409, 'eval_not_ready', 'question `Unknown`: expected_leaf is not a leaf store'),
			)
			.mockResolvedValueOnce(fx('run_started'));
		fakeApi({ runEval } as Partial<Api>);
		render(EvalPage);
		await screen.findByRole('table', { name: 'Eval questions' });
		await fireEvent.change(screen.getByLabelText('Store'), {
			target: { value: '7f1c9a2e-0000-4000-8000-0000000000a0' },
		});
		await fireEvent.click(screen.getByLabelText('Laya routing'));
		await fireEvent.click(screen.getByLabelText('Embedding'));
		await fireEvent.input(screen.getByLabelText('Branches (beam)'), { target: { value: '4' } });
		await fireEvent.click(screen.getByRole('button', { name: 'Run eval' }));
		await flush();
		expect(runEval).toHaveBeenCalledWith({
			root_id: '7f1c9a2e-0000-4000-8000-0000000000a0',
			modes: ['laya', 'hybrid'],
			beam: 4,
			min_score: undefined,
		});
		expect(screen.getByText('Not ready to evaluate')).toBeTruthy();
		expect(screen.getByText(/expected_leaf is not a leaf store/)).toBeTruthy();
		await fireEvent.click(screen.getByRole('button', { name: 'Run eval' }));
		await flush();
		const hits = await screen.findByRole('table', { name: 'Per-question hits' });
		expect(within(hits).getAllByText('hit #1').length).toBeGreaterThan(0);
		expect(within(hits).getAllByText('#2').length).toBe(2);
		expect(screen.queryByText('Not ready to evaluate')).toBeNull();
	});

	it('follows a started run: polls every 2 s, shows progress, then the results', async () => {
		vi.useFakeTimers({ shouldAdvanceTime: true });
		try {
			session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
			const done = fx<EvalRun>('run');
			const running: EvalRun = {
				...structuredClone(done),
				status: 'running',
				progress: { done: 1, total: 4 },
				verdict: null,
				finished_at: null,
				took_ms: null,
				reports: null,
				results: [],
			};
			const evalRun = vi
				.fn()
				.mockResolvedValueOnce(running) // open() right after the start
				.mockResolvedValueOnce({ ...running, progress: { done: 3, total: 4 } }) // first poll
				.mockResolvedValue(done);
			const api = fakeApi({ evalRun } as Partial<Api>);
			render(EvalPage);
			await screen.findByRole('table', { name: 'Eval questions' });
			await fireEvent.click(screen.getByRole('button', { name: 'Run eval' }));
			await flush();
			expect(api.runEval).toHaveBeenCalledTimes(1);
			expect(screen.getByTestId('run-progress').textContent).toContain('Running');
			expect((screen.getByRole('button', { name: 'Running…' }) as HTMLButtonElement).disabled).toBe(true);
			expect(screen.queryByRole('table', { name: 'Per-question hits' })).toBeNull();
			await vi.advanceTimersByTimeAsync(2000);
			await flush();
			expect(screen.getByTestId('run-progress').textContent).toContain('3 / 4 searches');
			await vi.advanceTimersByTimeAsync(2000);
			await flush();
			expect(await screen.findByRole('table', { name: 'Per-question hits' })).toBeTruthy();
			expect(screen.queryByTestId('run-progress')).toBeNull();
			expect(toastState.msg).toBe('Eval finished: PASS.');
			expect(evalRun).toHaveBeenCalledTimes(3);
			await vi.advanceTimersByTimeAsync(4000);
			expect(evalRun).toHaveBeenCalledTimes(3);
		} finally {
			vi.useRealTimers();
		}
	});

	it('picks up a running run from the list after a reload and shows why it failed', async () => {
		vi.useFakeTimers({ shouldAdvanceTime: true });
		try {
			session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
			const started = fx<EvalRunSummary>('run_started');
			const done = fx<EvalRun>('run');
			const failed: EvalRun = {
				...structuredClone(done),
				id: started.id,
				status: 'failed',
				error: 'laya is unavailable',
				verdict: null,
				reports: null,
				results: [],
			};
			const evalRun = vi.fn(async () => failed);
			const evalRuns = vi
				.fn()
				.mockResolvedValueOnce({ items: [started], next: null })
				.mockResolvedValue({ items: [withRun(started, failed)], next: null });
			fakeApi({ evalRuns, evalRun } as Partial<Api>);
			render(EvalPage);
			const runs = await screen.findByRole('table', { name: 'Eval runs' });
			expect(within(runs).getByText('running 0/4')).toBeTruthy();
			expect(screen.getByTestId('run-progress')).toBeTruthy();
			await vi.advanceTimersByTimeAsync(2000);
			await flush();
			expect(evalRun).toHaveBeenCalledWith(started.id);
			expect(within(runs).getByText('failed')).toBeTruthy();
			expect(toastState.msg).toBe('Eval failed: laya is unavailable');
			expect(toastState.kind).toBe('err');
			await fireEvent.click(within(runs).getByRole('button', { name: 'Open' }));
			await flush();
			expect(screen.getByTestId('run-detail').textContent).toContain('laya is unavailable');
		} finally {
			vi.useRealTimers();
		}
	});

	it('a tenant_user reads but cannot write or run', async () => {
		session.set({ sub: 'u1', name: 'U' }, meAs('tenant_user'));
		fakeApi();
		render(EvalPage);
		await screen.findByRole('table', { name: 'Eval questions' });
		for (const name of ['Add question', 'Run eval', 'Edit How long does a refund take?'])
			expect((screen.getAllByRole('button', { name })[0] as HTMLButtonElement).disabled).toBe(true);
	});

	it('compares the two ticked runs, older as A', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
		const older = fx<EvalRun>('run');
		const newer: EvalRun = {
			...structuredClone(older),
			id: 'run-new',
			started_at: '2026-10-20T09:00:00.000000Z',
			reports: older.reports!.map((r) => ({ ...r, recall_at_1: 1 })),
			results: older.results.map((r) => ({ ...r, rank: 1 })),
		};
		const evalRun = vi.fn(async (id: string) => (id === 'run-new' ? newer : older));
		fakeApi({
			// a listed run is a summary: open/compare must read the full runs
			evalRuns: vi.fn(async () => ({
				items: [newer, older].map(({ questions, results: _r, ...head }) => ({
					...head,
					question_count: questions.length,
				})),
				next: null,
			})),
			evalRun,
		} as Partial<Api>);
		render(EvalPage);
		const runs = await screen.findByRole('table', { name: 'Eval runs' });
		const boxes = within(runs).getAllByRole('checkbox');
		await fireEvent.click(boxes[0]!);
		await fireEvent.click(boxes[1]!);
		await fireEvent.click(screen.getByRole('button', { name: 'Compare selected' }));
		await flush();
		const cmp = await screen.findByTestId('run-compare');
		expect(cmp.textContent).toContain('2 better, 0 worse');
		const recall = within(cmp).getByRole('table', { name: 'Recall change' });
		expect(within(recall).getAllByText('+50 pts').length).toBe(2);
	});

	describe('a poll that fails', () => {
		const followListed = async (evalRun: Api['evalRun'], later: EvalRunSummary[] = []) => {
			session.set({ sub: 'u1', name: 'A' }, meAs('tenant_admin'));
			const started = fx<EvalRunSummary>('run_started');
			const evalRuns = vi
				.fn()
				.mockResolvedValueOnce({ items: [started], next: null })
				.mockResolvedValue({ items: later, next: null });
			fakeApi({ evalRuns, evalRun } as Partial<Api>);
			render(EvalPage);
			await screen.findByRole('table', { name: 'Eval runs' });
			expect(screen.getByTestId('run-progress')).toBeTruthy();
			return { started, evalRuns };
		};
		const runBtn = () => screen.getByRole('button', { name: /Run eval|Running…/ }) as HTMLButtonElement;

		it('stops on 404: the run no longer exists, and Run is usable again', async () => {
			vi.useFakeTimers({ shouldAdvanceTime: true });
			try {
				const evalRun = vi.fn(async () => {
					throw new ApiError(404, 'not_found', 'eval run not found');
				});
				const { evalRuns } = await followListed(evalRun);
				expect(runBtn().disabled).toBe(true);
				await vi.advanceTimersByTimeAsync(2000);
				await flush();
				expect(evalRun).toHaveBeenCalledTimes(1);
				expect(screen.getByTestId('poll-error').textContent).toContain('This run no longer exists.');
				expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull();
				expect(screen.queryByTestId('run-progress')).toBeNull();
				expect(runBtn().textContent).toBe('Run eval');
				expect(runBtn().disabled).toBe(false);
				expect(evalRuns).toHaveBeenCalledTimes(2);
				await vi.advanceTimersByTimeAsync(60_000);
				expect(evalRun).toHaveBeenCalledTimes(1);
			} finally {
				vi.useRealTimers();
			}
		});

		it.each([
			[401, 'Your session has expired. Sign in again to follow it.'],
			[403, 'You no longer have permission to read this. Ask a tenant admin.'],
		])('stops on %i and says why', async (status, message) => {
			vi.useFakeTimers({ shouldAdvanceTime: true });
			try {
				const evalRun = vi.fn(async () => {
					throw new ApiError(status, status === 401 ? 'unauthorized' : 'forbidden', 'no');
				});
				await followListed(evalRun);
				await vi.advanceTimersByTimeAsync(2000);
				await flush();
				expect(screen.getByTestId('poll-error').textContent).toContain(message);
				expect(screen.queryByRole('button', { name: 'Retry' })).toBeNull();
				expect(runBtn().disabled).toBe(false);
				await vi.advanceTimersByTimeAsync(60_000);
				expect(evalRun).toHaveBeenCalledTimes(1);
			} finally {
				vi.useRealTimers();
			}
		});

		it('backs off 2 s doubling to 30 s on other errors, stops after 6 failures, and Retry follows it again', async () => {
			vi.useFakeTimers({ shouldAdvanceTime: true });
			try {
				const done = fx<EvalRun>('run');
				const evalRun = vi.fn(async (_id: string): Promise<EvalRun> => {
					throw new ApiError(503, 'upstream_unavailable', 'strata_rag is unavailable');
				});
				const first = fx<EvalRunSummary>('run_started');
				const { started } = await followListed(evalRun, [withRun(first, { ...done, id: first.id })]);
				// the poll waits 2 s, then 2, 4, 8, 16 and 30 s between failed reads (checked at 90% and 100% of each
				// wait: the clock also moves with real time, so an exact edge would be flaky)
				for (const [wait, calls] of [
					[2000, 1],
					[2000, 2],
					[4000, 3],
					[8000, 4],
					[16000, 5],
					[30000, 6],
				] as const) {
					await vi.advanceTimersByTimeAsync(wait * 0.9);
					expect(evalRun).toHaveBeenCalledTimes(calls - 1);
					await vi.advanceTimersByTimeAsync(wait * 0.1);
					await flush();
					expect(evalRun).toHaveBeenCalledTimes(calls);
					if (calls < POLL_MAX_FAILURES) expect(screen.getByTestId('run-progress')).toBeTruthy();
				}
				expect(evalRun).toHaveBeenCalledTimes(POLL_MAX_FAILURES);
				expect(screen.getByTestId('poll-error').textContent).toContain(
					'Lost track of this run after 6 failed reads: strata_rag is unavailable',
				);
				expect(runBtn().disabled).toBe(false);
				await vi.advanceTimersByTimeAsync(120_000);
				expect(evalRun).toHaveBeenCalledTimes(POLL_MAX_FAILURES);

				evalRun.mockImplementation(async () => ({ ...done, id: started.id }));
				await fireEvent.click(screen.getByRole('button', { name: 'Retry' }));
				expect(screen.queryByTestId('poll-error')).toBeNull();
				expect(screen.getByTestId('run-progress')).toBeTruthy();
				await vi.advanceTimersByTimeAsync(2000);
				await flush();
				expect(evalRun).toHaveBeenCalledTimes(POLL_MAX_FAILURES + 1);
				expect(toastState.msg).toBe('Eval finished: PASS.');
				expect(screen.queryByTestId('run-progress')).toBeNull();
			} finally {
				vi.useRealTimers();
			}
		});

		it('a read that succeeds resets the back-off', async () => {
			vi.useFakeTimers({ shouldAdvanceTime: true });
			try {
				const started = fx<EvalRunSummary>('run_started');
				const running = { ...fx<EvalRun>('run'), id: started.id, status: 'running' as const, results: [] };
				const fail = new ApiError(503, 'upstream_unavailable', 'down');
				const evalRun = vi
					.fn()
					.mockRejectedValueOnce(fail) // t=2 s
					.mockRejectedValueOnce(fail) // t=4 s
					.mockResolvedValueOnce(running) // t=8 s
					.mockRejectedValue(fail); // t=10 s: the first failure of a new streak waits 2 s again
				await followListed(evalRun, [started]);
				await vi.advanceTimersByTimeAsync(2000 + 2000 + 4000);
				await flush();
				expect(evalRun).toHaveBeenCalledTimes(3);
				await vi.advanceTimersByTimeAsync(2000);
				await flush();
				expect(evalRun).toHaveBeenCalledTimes(4);
				await vi.advanceTimersByTimeAsync(2000);
				await flush();
				expect(evalRun).toHaveBeenCalledTimes(5);
			} finally {
				vi.useRealTimers();
			}
		});
	});
});

describe('afterPollError', () => {
	it('404 is gone; 401 and 403 stop with why; anything else backs off 2 s doubling to 30 s, then gives up', () => {
		expect(afterPollError(new ApiError(404, 'not_found', 'x'), 1, 2000)).toEqual({ kind: 'gone' });
		expect(afterPollError(new ApiError(401, 'unauthorized', 'x'), 1, 2000)).toMatchObject({ kind: 'auth' });
		expect(afterPollError(new ApiError(403, 'forbidden', 'x'), 6, 2000)).toMatchObject({ kind: 'auth' });
		const delays = [1, 2, 3, 4, 5].map((n) => afterPollError(new TypeError('Failed to fetch'), n, 2000));
		expect(delays).toEqual([2000, 4000, 8000, 16000, 30000].map((delay) => ({ kind: 'retry', delay })));
		expect(afterPollError(new ApiError(502, 'upstream_unavailable', 'down'), 6, 2000)).toEqual({
			kind: 'give_up',
			message: 'down',
		});
		expect(afterPollError('weird', POLL_MAX_FAILURES, 2000)).toEqual({ kind: 'give_up', message: 'weird' });
	});
});
