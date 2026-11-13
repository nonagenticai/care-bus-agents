// The gates page's pure helpers, with inline inputs (ruling F7): rule summaries, the fire rule (spec §3 Firing),
// criteria validation (laya: noul exactly true/false, choice 2–255 options, score 2–10 levels) and the request layer.
import { describe, expect, it, vi } from 'vitest';
import {
	answerValue,
	byPoint,
	createGatesApi,
	criteriaFor,
	isSkipped,
	newQuestion,
	parseDetails,
	parseTools,
	playgroundState,
	probabilityRows,
	ruleSummary,
	skippedKeys,
	toInput,
	validateCriteria,
	validateQuestion,
	verdictItems,
	wouldFire,
	type GateQuestion,
} from '../../src/lib/api/gates';
import { ApiError } from '../../src/lib/api/client';

const q = (over: Partial<GateQuestion>): GateQuestion => ({
	...newQuestion('request', 0),
	key: 'k',
	...over,
});
const emergency = q({
	key: 'emergency',
	instructions: 'Is someone in danger?',
	criteria: { true: 'danger', false: 'no danger' },
	threshold: 0.7,
});
const frustration = q({
	key: 'frustration',
	type: 'score',
	instructions: 'How frustrated?',
	criteria: ['calm', 'mild', 'annoyed', 'angry', 'furious'],
	threshold: 3,
	fail: 'open',
});
const topic = q({
	key: 'topic',
	type: 'choice',
	instructions: 'Topic?',
	criteria: { returns: 'a return', warranty: 'a repair', other: 'anything else' },
	fire_when: 'in',
	threshold: null,
	fire_values: ['warranty'],
	action: 'annotate',
	fail: 'open',
});

describe('ruleSummary', () => {
	it('names the value, operator, threshold, action and fail policy', () => {
		expect(ruleSummary(emergency)).toBe('fires when noul ≥ 0.7 → handoff, fail-closed');
		expect(ruleSummary({ ...emergency, fire_when: 'lt', threshold: 0.5, action: 'block' })).toBe(
			'fires when noul < 0.5 → block, fail-closed',
		);
	});
	it('labels a score threshold with its level', () => {
		expect(ruleSummary(frustration)).toBe('fires when score ≥ 3 (angry) → handoff, fail-open');
	});
	it('compares confidence for a choice threshold and lists fire_values for `in`', () => {
		expect(ruleSummary({ ...topic, fire_when: 'gte', threshold: 0.9, fire_values: null })).toBe(
			'fires when confidence ≥ 0.9 → annotate, fail-open',
		);
		expect(ruleSummary(topic)).toBe('fires when choice is warranty → annotate, fail-open');
		expect(ruleSummary({ ...topic, fire_values: ['warranty', 'other'] })).toBe(
			'fires when choice is one of {warranty, other} → annotate, fail-open',
		);
	});
	it('`in` with a minimum confidence names it after the option', () => {
		const yes = q({
			type: 'choice',
			criteria: { yes: 'a refund is promised', no: 'it is not' },
			fire_when: 'in',
			fire_values: ['yes'],
			threshold: 0.6,
			action: 'block',
			fail: 'closed',
		});
		expect(ruleSummary(yes)).toBe('fires when choice is yes with confidence ≥ 0.6 → block, fail-closed');
		expect(ruleSummary({ ...yes, threshold: Number.NaN })).toBe(
			'fires when choice is yes → block, fail-closed',
		);
	});
});

describe('wouldFire', () => {
	it('noul: gte fires at and above the threshold, not below', () => {
		expect(wouldFire(emergency, { noul: 0.7 })).toBe(true);
		expect(wouldFire(emergency, { noul: 0.69 })).toBe(false);
	});
	it('noul lt (customer_consent): fires below the threshold only', () => {
		const consent = { ...emergency, fire_when: 'lt' as const, threshold: 0.5 };
		expect(wouldFire(consent, { noul: 0.2 })).toBe(true);
		expect(wouldFire(consent, { noul: 0.5 })).toBe(false);
	});
	it('score compares the level index', () => {
		expect(wouldFire(frustration, { score: 3, confidence: 0.4 })).toBe(true);
		expect(wouldFire(frustration, { score: 2, confidence: 0.99 })).toBe(false);
	});
	it('choice threshold compares confidence, not the option', () => {
		const c = { ...topic, fire_when: 'gte' as const, threshold: 0.8 };
		expect(wouldFire(c, { choice: 'other', confidence: 0.81 })).toBe(true);
		expect(wouldFire(c, { choice: 'warranty', confidence: 0.79 })).toBe(false);
	});
	it('`in` fires on membership of the chosen option', () => {
		expect(wouldFire(topic, { choice: 'warranty', confidence: 0.1 })).toBe(true);
		expect(wouldFire(topic, { choice: 'returns', confidence: 0.99 })).toBe(false);
	});
	it('`in` with a threshold also needs the confidence in the chosen option to reach it', () => {
		const t = { ...topic, threshold: 0.6 };
		expect(wouldFire(t, { choice: 'warranty', confidence: 0.6 })).toBe(true);
		expect(wouldFire(t, { choice: 'warranty', confidence: 0.59 })).toBe(false);
		expect(wouldFire(t, { choice: 'returns', confidence: 0.99 })).toBe(false);
		expect(wouldFire(t, { choice: 'warranty' })).toBeNull();
		expect(wouldFire({ ...topic, threshold: null }, { choice: 'warranty' })).toBe(true);
	});
	it('cannot tell without an answer or the value it compares', () => {
		expect(wouldFire(emergency, null)).toBeNull();
		expect(wouldFire(emergency, { score: 1 })).toBeNull();
		expect(wouldFire(topic, { confidence: 0.9 })).toBeNull();
		expect(answerValue('noul', { noul: Number.NaN })).toBeNull();
	});
});

describe('probabilityRows', () => {
	it('lists choice probabilities by option', () => {
		expect(
			probabilityRows('choice', { probabilities: { returns: 0.2, warranty: 0.8 } }, topic.criteria),
		).toEqual([
			['returns', 0.2],
			['warranty', 0.8],
		]);
	});
	it('labels an indexed score distribution with its levels', () => {
		expect(probabilityRows('score', { probabilities: [0.1, 0.9] }, ['calm', 'angry'])).toEqual([
			['0 · calm', 0.1],
			['1 · angry', 0.9],
		]);
	});
	it('is empty when laya sent no distribution', () => {
		expect(probabilityRows('noul', { noul: 0.3 }, emergency.criteria)).toEqual([]);
	});
});

describe('validateCriteria', () => {
	it('noul: exactly true and false, both described', () => {
		expect(validateCriteria('noul', { true: 'a', false: 'b' })).toBeNull();
		expect(validateCriteria('noul', { true: 'a' })).toMatch(/exactly two/);
		expect(validateCriteria('noul', { true: 'a', false: 'b', maybe: 'c' })).toMatch(/exactly two/);
		expect(validateCriteria('noul', { true: 'a', false: ' ' })).toMatch(/both/);
		expect(validateCriteria('noul', ['a', 'b'])).toMatch(/true and a false/);
	});
	it('choice: 2–255 described options', () => {
		const opts = (n: number) => Object.fromEntries(Array.from({ length: n }, (_, i) => [`o${i}`, 'd']));
		expect(validateCriteria('choice', opts(1))).toMatch(/2–255.*now 1/);
		expect(validateCriteria('choice', opts(2))).toBeNull();
		expect(validateCriteria('choice', opts(255))).toBeNull();
		expect(validateCriteria('choice', opts(256))).toMatch(/now 256/);
		expect(validateCriteria('choice', { a: 'x', b: '' })).toMatch(/description/);
		expect(validateCriteria('choice', ['a', 'b'])).toMatch(/needs options/);
	});
	it('score: 2–10 distinct, non-blank levels', () => {
		const lv = (n: number) => Array.from({ length: n }, (_, i) => `l${i}`);
		expect(validateCriteria('score', lv(1))).toMatch(/now 1/);
		expect(validateCriteria('score', lv(2))).toBeNull();
		expect(validateCriteria('score', lv(10))).toBeNull();
		expect(validateCriteria('score', lv(11))).toMatch(/now 11/);
		expect(validateCriteria('score', ['a', ''])).toMatch(/label/);
		expect(validateCriteria('score', ['a', 'a'])).toMatch(/distinct/);
		expect(validateCriteria('score', { a: 'x', b: 'y' })).toMatch(/ordered list/);
	});
});

describe('validateQuestion', () => {
	it('accepts the seeded shapes', () => {
		expect(validateQuestion(emergency, false)).toEqual({});
		expect(validateQuestion(frustration, false)).toEqual({});
		expect(validateQuestion(topic, false)).toEqual({});
	});
	it('rejects a bad or duplicate key on create only', () => {
		expect(validateQuestion({ ...emergency, key: 'Bad Key' }, true).key).toBeTruthy();
		expect(validateQuestion(emergency, true, ['emergency']).key).toMatch(/already exists/);
		expect(validateQuestion(emergency, false, ['emergency']).key).toBeUndefined();
	});
	it('bounds thresholds: probability 0..1, score a level index', () => {
		expect(validateQuestion({ ...emergency, threshold: 1.5 }, false).threshold).toBeTruthy();
		expect(validateQuestion({ ...emergency, threshold: null }, false).threshold).toBeTruthy();
		expect(validateQuestion({ ...frustration, threshold: 5 }, false).threshold).toMatch(/0–4/);
		expect(validateQuestion({ ...frustration, threshold: 1.5 }, false).threshold).toBeTruthy();
	});
	it('`in` needs choice options that exist', () => {
		expect(validateQuestion({ ...topic, fire_values: [] }, false).fire_values).toBeTruthy();
		expect(validateQuestion({ ...topic, fire_values: ['nope'] }, false).fire_values).toMatch(/nope/);
		expect(
			validateQuestion({ ...emergency, fire_when: 'in', fire_values: ['x'] }, false).fire_values,
		).toMatch(/only applies/);
	});
	it('`in` takes an optional minimum confidence between 0 and 1', () => {
		expect(validateQuestion({ ...topic, threshold: null }, false)).toEqual({});
		expect(validateQuestion({ ...topic, threshold: 0.6 }, false)).toEqual({});
		expect(validateQuestion({ ...topic, threshold: 1.2 }, false).threshold).toMatch(/between 0 and 1/);
		expect(validateQuestion({ ...topic, threshold: -0.1 }, false).threshold).toMatch(/between 0 and 1/);
	});
	it('needs instructions and keeps tools to the tool point', () => {
		expect(validateQuestion({ ...emergency, instructions: ' ' }, false).instructions).toBeTruthy();
		expect(validateQuestion({ ...emergency, tools: ['x'] }, false).tools).toBeTruthy();
		expect(validateQuestion({ ...emergency, point: 'tool', tools: ['x'] }, false).tools).toBeUndefined();
	});
});

describe('input and precheck', () => {
	it('only request questions read the customer message alone; a precheck is at most 500 characters', () => {
		expect(validateQuestion({ ...emergency, input: 'message', precheck: '(?i)\\d{4,}' }, false)).toEqual({});
		expect(validateQuestion({ ...emergency, point: 'tool', input: 'message' }, false).input).toMatch(
			/request-point/,
		);
		expect(validateQuestion({ ...emergency, precheck: 'a'.repeat(501) }, false).precheck).toMatch(/500/);
		expect(validateQuestion({ ...emergency, precheck: 'a'.repeat(500) }, false).precheck).toBeUndefined();
	});
	it('toInput sends input and a trimmed precheck (blank: none), and request off the request point', () => {
		expect(toInput(emergency)).toMatchObject({ input: 'request', precheck: null });
		expect(toInput({ ...emergency, input: 'message', precheck: '  (?i)pin  ' })).toMatchObject({
			input: 'message',
			precheck: '(?i)pin',
		});
		expect(toInput({ ...emergency, precheck: '   ' }).precheck).toBeNull();
		expect(toInput({ ...emergency, point: 'response', input: 'message' }).input).toBe('request');
		// an older server's question has neither field
		const { input: _i, precheck: _p, ...old } = emergency;
		expect(toInput(old as GateQuestion)).toMatchObject({ input: 'request', precheck: null });
	});
	it('reads precheck-skipped answers', () => {
		const answers = {
			credentials_shared: { type: 'choice', fired: false, skipped: 'precheck' },
			emergency: { noul: 0.1, fired: false },
		};
		expect(isSkipped(answers.credentials_shared)).toBe(true);
		expect(isSkipped(answers.emergency)).toBe(false);
		expect(isSkipped(null)).toBe(false);
		expect(skippedKeys(answers)).toEqual(['credentials_shared']);
		expect(skippedKeys(null)).toEqual([]);
	});
	it('the request playground sends the draft when a request type is set', () => {
		expect(playgroundState('request', 'ORD-1042')).toEqual({ point: 'request', state: 'ORD-1042' });
		expect(playgroundState('request', 'ORD-1042', '', { type: '  ' })).toEqual({
			point: 'request',
			state: 'ORD-1042',
		});
		expect(
			playgroundState('request', 'ORD-1042', '', { type: ' delivery ', details: { order_id: 'ORD-1042' } }),
		).toEqual({
			point: 'request',
			state: 'ORD-1042',
			request: { type: 'delivery', details: { order_id: 'ORD-1042' } },
		});
		expect(parseDetails('')).toEqual({});
		expect(parseDetails('{"a": 1}')).toEqual({ a: 1 });
		expect(parseDetails('[1]')).toMatch(/JSON object/);
		expect(parseDetails('{oops')).toMatch(/JSON object/);
	});
});

describe('editor helpers', () => {
	it('criteriaFor gives each type a valid starting shape', () => {
		// A new choice question starts with two options to describe; a score with two valid levels.
		expect(Object.keys(criteriaFor('choice'))).toHaveLength(2);
		expect(validateCriteria('choice', criteriaFor('choice'))).toMatch(/description/);
		expect(validateCriteria('score', criteriaFor('score'))).toBeNull();
		expect(criteriaFor('noul', { true: 'kept', false: 'also' })).toEqual({ true: 'kept', false: 'also' });
		expect(criteriaFor('score', ['a', 'b', 'c'])).toEqual(['a', 'b', 'c']);
	});
	it('toInput drops the key, fire_values outside `in`, threshold inside `in`, tools off the tool point', () => {
		const body = toInput({ ...emergency, fire_values: ['x'], tools: ['t'], message: '  ' });
		expect(body).not.toHaveProperty('key');
		expect(body).toMatchObject({ fire_values: null, tools: null, message: null, threshold: 0.7 });
		expect(toInput(topic)).toMatchObject({ threshold: null, fire_values: ['warranty'] });
		expect(toInput({ ...topic, threshold: 0.6 })).toMatchObject({
			threshold: 0.6,
			fire_values: ['warranty'],
		});
		expect(toInput({ ...topic, threshold: Number.NaN }).threshold).toBeNull();
		expect(toInput({ ...topic, threshold: undefined as unknown as null }).threshold).toBeNull();
		expect(toInput({ ...emergency, point: 'tool', tools: ['order_issue_refund'] }).tools).toEqual([
			'order_issue_refund',
		]);
	});
	it('byPoint groups and orders by ordinal then key', () => {
		const g = byPoint([
			{ ...emergency, key: 'b', ordinal: 1 },
			{ ...emergency, key: 'a', ordinal: 1 },
			{ ...emergency, key: 'z', ordinal: 0 },
			{ ...emergency, key: 't', point: 'tool' },
		]);
		expect(g.request.map((x) => x.key)).toEqual(['z', 'a', 'b']);
		expect(g.tool.map((x) => x.key)).toEqual(['t']);
		expect(g.response).toEqual([]);
	});
	it('parseTools splits, trims and deduplicates', () => {
		expect(parseTools(' a, b\nc ,a,, ')).toEqual(['a', 'b', 'c']);
	});
	it('playgroundState sends only the customer words at the tool point, with the tool separate', () => {
		expect(playgroundState('response', 'Your refund is on its way')).toEqual({
			point: 'response',
			state: 'Your refund is on its way',
		});
		expect(playgroundState('tool', 'refund me', 'order_issue_refund')).toEqual({
			point: 'tool',
			state: 'refund me',
			tool: 'order_issue_refund',
		});
	});
	it('verdictItems accepts a list, a page or a verdicts envelope', () => {
		const v = [{ id: 1 }];
		expect(verdictItems(v)).toEqual(v);
		expect(verdictItems({ items: v, next: null })).toEqual(v);
		expect(verdictItems({ verdicts: v })).toEqual(v);
		expect(verdictItems(null)).toEqual([]);
	});
});

describe('createGatesApi', () => {
	const json = (status: number, body: unknown) =>
		new Response(JSON.stringify(body), { status, headers: { 'content-type': 'application/json' } });
	const mk = (fetch: typeof globalThis.fetch, token = vi.fn(async () => 't')) =>
		createGatesApi({ baseUrl: 'http://api', token, job: () => 'care job', activeTenant: () => 'ten', fetch });

	it('sends job-scoped routes with the token and active tenant', async () => {
		const f = vi.fn(async () => json(200, { questions: [], provider: { bound: false, model: null } }));
		await mk(f as unknown as typeof fetch).list();
		const [url, init] = f.mock.calls[0] as unknown as [string, RequestInit];
		expect(url).toBe('http://api/api/jobs/care%20job/gates');
		expect(init.headers).toMatchObject({ Authorization: 'Bearer t', 'x-active-tenant-id': 'ten' });
	});
	it('PUTs a question by key and POSTs evaluate with the body', async () => {
		const f = vi.fn(async () => json(200, {}));
		const g = mk(f as unknown as typeof fetch);
		await g.save('emergency', toInput(emergency));
		await g.evaluate({ point: 'request', state: 'hi' });
		const calls = f.mock.calls as unknown as [string, RequestInit][];
		expect(calls[0]![0]).toBe('http://api/api/jobs/care%20job/gates/emergency');
		expect(calls[0]![1].method).toBe('PUT');
		expect(calls[1]![0]).toBe('http://api/api/jobs/care%20job/gates/evaluate');
		expect(JSON.parse(String(calls[1]![1].body))).toEqual({ point: 'request', state: 'hi' });
	});
	it('retries once with a forced token on 401 and maps the error envelope', async () => {
		const token = vi.fn(async () => 't');
		const f = vi.fn(async () => json(401, { error: { code: 'unauthorized', message: 'nope' } }));
		const err = await mk(f as unknown as typeof fetch, token)
			.verdicts()
			.catch((e) => e);
		expect(f).toHaveBeenCalledTimes(2);
		expect(token).toHaveBeenLastCalledWith({ force: true });
		expect(err).toBeInstanceOf(ApiError);
		expect(err).toMatchObject({ status: 401, code: 'unauthorized', message: 'nope' });
	});
	it('reads verdicts with ?limit= and treats 204 as no body', async () => {
		const f = vi.fn(async (url: string) =>
			url.includes('verdicts')
				? json(200, { items: [{ id: 7 }], next: null })
				: new Response(null, { status: 204 }),
		);
		const g = mk(f as unknown as typeof fetch);
		expect(await g.verdicts(50)).toEqual([{ id: 7 }]);
		expect((f.mock.calls[0] as unknown as [string])[0]).toMatch(/\/gates\/verdicts\?limit=50$/);
		expect(await g.remove('x')).toBeUndefined();
	});
	it('reports an unreachable server as status 0', async () => {
		const f = vi.fn(async () => {
			throw new TypeError('Failed to fetch');
		});
		await expect(mk(f as unknown as typeof fetch).list()).rejects.toMatchObject({
			status: 0,
			code: 'unavailable',
		});
	});
});
