import { describe, expect, it } from 'vitest';
import { load } from '../../src/lib/load.svelte';
import { ApiError } from '../../src/lib/api/client';
import { guard } from '../../src/lib/app.svelte';
import { toastState } from '../../src/lib/ui/toaster.svelte';

describe('load()', () => {
	it('returns ok with data', async () => {
		expect(await load(async () => [1, 2])).toEqual({ state: 'ok', data: [1, 2] });
	});
	it('returns empty for an empty array or an empty Paged by default, or when the predicate says so', async () => {
		expect(await load(async () => [])).toEqual({ state: 'empty', data: [] });
		expect(await load(async () => ({ items: [], next: null }))).toEqual({
			state: 'empty',
			data: { items: [], next: null },
		});
		expect(await load(async () => ({ items: [1], next: null }))).toEqual({
			state: 'ok',
			data: { items: [1], next: null },
		});
		expect(
			await load(
				async () => ({ rows: [] }),
				(d) => d.rows.length === 0,
			),
		).toEqual({ state: 'empty', data: { rows: [] } });
	});
	it('returns unavailable with the reason for ApiError, never empty or ok', async () => {
		const r = await load(async () => {
			throw new ApiError(503, 'dependency_unavailable', 'strata_rag timed out after 8000 ms');
		});
		expect(r).toEqual({ state: 'unavailable', reason: 'strata_rag timed out after 8000 ms', status: 503 });
	});
});

describe('guard()', () => {
	it('resolves true and shows the success message', async () => {
		expect(await guard(async () => 'ok', 'Saved.')).toBe(true);
		expect(toastState.msg).toBe('Saved.');
		expect(toastState.visible).toBe(true);
	});
	it('resolves false on a 403 and shows the tenant_admin message', async () => {
		const ok = await guard(async () => {
			throw new ApiError(403, 'forbidden', 'forbidden');
		}, 'Saved.');
		expect(ok).toBe(false);
		expect(toastState.msg).toBe('You need the tenant_admin role to do this.');
	});
	it('resolves false on an unreachable API and on other errors', async () => {
		expect(
			await guard(async () => {
				throw new ApiError(0, 'unavailable', 'down');
			}, 'Saved.'),
		).toBe(false);
		expect(toastState.msg).toBe(
			'The write outcome is uncertain: no answer came back. Reload before trying again; the server may have accepted it.',
		);
		expect(
			await guard(async () => {
				throw new ApiError(422, 'invalid', 'name too long');
			}, 'Saved.'),
		).toBe(false);
		expect(toastState.msg).toBe('Not saved: name too long');
		// A 503 is a definite refusal: the server's own message is shown, not "uncertain".
		expect(
			await guard(async () => {
				throw new ApiError(503, 'upstream_unavailable', 'strata_rag is down');
			}, 'Saved.'),
		).toBe(false);
		expect(toastState.msg).toBe('Not saved: strata_rag is down');
	});
});

describe('Resource.refresh() latest-wins', () => {
	it('ignores a response that is not from the latest call', async () => {
		const { Resource } = await import('../../src/lib/load.svelte');
		const gates: Array<(v: string[]) => void> = [];
		const r = new Resource<string[]>(() => new Promise((res) => gates.push(res)));
		const first = r.refresh();
		const second = r.refresh();
		gates[1]!(['new']);
		await second;
		gates[0]!(['old']);
		await first;
		expect(r.value).toEqual({ state: 'ok', data: ['new'] });
	});
});

describe('latest()', () => {
	it('marks earlier tickets stale', async () => {
		const { Latest } = await import('../../src/lib/load.svelte');
		const l = new Latest();
		const a = l.next();
		const b = l.next();
		expect(l.stale(a)).toBe(true);
		expect(l.stale(b)).toBe(false);
	});
});

describe('Resource.retry', () => {
	it('is a bound refresh a consumer can hand to <Unavailable retry>', async () => {
		const { Resource } = await import('../../src/lib/load.svelte');
		let fail = true;
		const r = new Resource<string[]>(async () => {
			if (fail) throw new ApiError(503, 'dependency_unavailable', 'down');
			return ['back'];
		});
		await r.refresh();
		expect(r.value.state).toBe('unavailable');
		fail = false;
		const { retry } = r;
		await retry();
		expect(r.value).toEqual({ state: 'ok', data: ['back'] });
	});
});

it('preserves continuation metadata on an empty page', async () => {
	const page = { items: [], next: 'older-cursor' };
	expect(await load(async () => page)).toEqual({ state: 'empty', data: page });
});
