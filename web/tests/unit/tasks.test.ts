// MCP tasks: the schema-driven answer form helpers (questionFields, validateAnswer) and the request layer.
import { describe, expect, it, vi } from 'vitest';
import { createClient } from '../../src/lib/api/client';
import {
	answerContent,
	isTaskTerminal,
	questionFields,
	taskStatusWord,
	validateAnswer,
	type JsonSchema,
} from '../../src/lib/api/tasks';

const SCHEMA: JsonSchema = {
	type: 'object',
	required: ['decision'],
	properties: {
		comment: { type: 'string' },
		reason: { type: 'string' },
		decision: { type: 'string', enum: ['approved', 'rejected'] },
		deblock_action: { type: 'string', enum: ['retry', 'skip'] },
	},
};

describe('questionFields', () => {
	it('orders decision, reason, comment first and classifies kinds', () => {
		const f = questionFields(SCHEMA);
		expect(f.map((x) => x.name)).toEqual(['decision', 'reason', 'comment', 'deblock_action']);
		expect(f[0]).toEqual({
			name: 'decision',
			kind: 'choice',
			options: ['approved', 'rejected'],
			required: true,
		});
		expect(f[1]).toMatchObject({ kind: 'text', options: [], required: false });
	});
	it('copes with a missing or empty schema', () => {
		expect(questionFields(null)).toEqual([]);
		expect(questionFields({})).toEqual([]);
	});
});

describe('validateAnswer', () => {
	it('accepts an approval', () => {
		expect(validateAnswer(SCHEMA, { decision: 'approved' })).toBeNull();
	});
	it('requires decision', () => {
		expect(validateAnswer(SCHEMA, {})).toBe('decision is required.');
		expect(validateAnswer(SCHEMA, { decision: '  ' })).toBe('decision is required.');
	});
	it('rejects a value outside the enum', () => {
		expect(validateAnswer(SCHEMA, { decision: 'maybe' })).toMatch(/must be one of: approved, rejected/);
	});
	it('requires a reason when rejected', () => {
		expect(validateAnswer(SCHEMA, { decision: 'rejected' })).toMatch(/reason is required/);
		expect(validateAnswer(SCHEMA, { decision: 'rejected', reason: ' ' })).toMatch(/reason is required/);
		expect(validateAnswer(SCHEMA, { decision: 'rejected', reason: 'too risky' })).toBeNull();
	});
	it('rejects unknown fields', () => {
		expect(validateAnswer(SCHEMA, { decision: 'approved', extra: 'x' })).toBe('Unknown field "extra".');
	});
	it('checks an optional enum only when given', () => {
		expect(validateAnswer(SCHEMA, { decision: 'approved', deblock_action: 'nope' })).toMatch(
			/deblock_action/,
		);
		expect(validateAnswer(SCHEMA, { decision: 'approved', deblock_action: 'skip' })).toBeNull();
	});
});

describe('helpers', () => {
	it('answerContent drops blanks and trims', () => {
		expect(answerContent({ decision: 'approved', reason: '  ', comment: ' ok ' })).toEqual({
			decision: 'approved',
			comment: 'ok',
		});
	});
	it('status helpers', () => {
		expect(isTaskTerminal('failed')).toBe(true);
		expect(isTaskTerminal('input_required')).toBe(false);
		expect(taskStatusWord('input_required')).toBe('open');
	});
});

describe('client', () => {
	it('calls the job-relative routes with encoded ids', async () => {
		type Call = [string, string, string | undefined];
		const calls: Call[] = [];
		const f = vi.fn(async (url: string, init?: RequestInit) => {
			calls.push([init?.method ?? 'GET', url, init?.body as string | undefined]);
			return new Response('{"items":[]}', { status: 200, headers: { 'content-type': 'application/json' } });
		});
		const api = createClient({
			baseUrl: '',
			token: async () => 't',
			job: () => 'j1',
			fetch: f as unknown as typeof fetch,
		});
		await api.mcpTasks('open');
		await api.answerMcpTask('a/b', { key: 'k', action: 'decline' });
		await api.cancelMcpTask('a/b');
		const [c0, c1, c2] = calls as [Call, Call, Call];
		expect(c0).toEqual(['GET', '/api/jobs/j1/mcp-tasks?state=open', undefined]);
		expect(c1[0]).toBe('POST');
		expect(c1[1]).toBe('/api/jobs/j1/mcp-tasks/a%2Fb/answers');
		expect(JSON.parse(c1[2]!)).toEqual({ key: 'k', action: 'decline' });
		expect(c2[1]).toBe('/api/jobs/j1/mcp-tasks/a%2Fb/cancel');
		expect(c2[2]).toBe('{}');
	});
});
