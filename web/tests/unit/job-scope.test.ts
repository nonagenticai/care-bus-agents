import { describe, expect, it, vi } from 'vitest';
import { jobPath, legacyAdminTarget, routeJob, switchJobPath } from '../../src/lib/scope';
import { createClient } from '../../src/lib/api/client';

describe('job-scoped admin URLs', () => {
	it('legacy admin URLs land on the same page of the job, keeping query and hash', () => {
		expect(legacyAdminTarget('/admin/cases', '?case=C1', '#x', 'j')).toBe('/admin/jobs/j/cases?case=C1#x');
		expect(legacyAdminTarget('/admin', '', '', 'j')).toBe('/admin/jobs/j');
		expect(legacyAdminTarget('/admin/', '', '', 'j')).toBe('/admin/jobs/j');
		expect(legacyAdminTarget('/admin/knowledge', '', '', 'j')).toBe('/admin/jobs/j/knowledge');
		expect(legacyAdminTarget('/admin/procedures/x', '', '', 'j')).toBe('/admin/jobs/j/procedures/x');
	});
	it('unknown or tenant-level paths are not redirected', () => {
		expect(legacyAdminTarget('/admin/nope', '', '', 'j')).toBeNull();
		expect(legacyAdminTarget('/admin/casesx', '', '', 'j')).toBeNull();
		expect(legacyAdminTarget('/admin/jobs', '', '', 'j')).toBeNull();
		expect(legacyAdminTarget('/console', '', '', 'j')).toBeNull();
	});
	it('switching job keeps the sub-page; off job pages there is nothing to switch', () => {
		expect(switchJobPath('/admin/jobs/a/ontology', 'b')).toBe('/admin/jobs/b/ontology');
		expect(switchJobPath('/admin/jobs/a', 'b')).toBe('/admin/jobs/b');
		expect(switchJobPath('/console', 'b')).toBeNull();
		expect(switchJobPath('/admin/users', 'b')).toBeNull();
	});
	it('encodes the job key and reads it only from job routes', () => {
		expect(jobPath('a b', '/x')).toBe('/admin/jobs/a%20b/x');
		expect(routeJob('/admin/jobs/[job]/cases', { job: 'k' })).toBe('k');
		expect(routeJob('/admin/jobs/[job]', { job: 'k' })).toBe('k');
		expect(routeJob('/admin/users', { job: 'k' })).toBeNull();
		expect(routeJob(null, {})).toBeNull();
	});
});

const ok = (body: unknown, status = 200) =>
	new Response(status === 204 ? null : JSON.stringify(body), {
		status,
		headers: { 'content-type': 'application/json' },
	});

describe('plan 14 client routes', () => {
	const mk = (res: Response = ok({ items: [], next: null })) => {
		const fetchFn = vi.fn().mockResolvedValue(res);
		const api = createClient({ baseUrl: 'https://api.test', token: async () => 't', fetch: fetchFn });
		return { api, fetchFn };
	};
	it('layer lists send ?layer= for one side only', async () => {
		const { api, fetchFn } = mk();
		await api.connectors('base');
		await api.connectors();
		await api.connectors('effective');
		await api.entities({ layer: 'tenant', includeHidden: false });
		await api.knowledgeStores('tenant');
		const urls = fetchFn.mock.calls.map((c) => c[0]);
		expect(urls).toEqual([
			'https://api.test/api/jobs/customer_care_agent/connectors?layer=base',
			'https://api.test/api/jobs/customer_care_agent/connectors',
			'https://api.test/api/jobs/customer_care_agent/connectors',
			'https://api.test/api/jobs/customer_care_agent/ontology/entities?layer=tenant',
			'https://api.test/api/jobs/customer_care_agent/knowledge-stores?layer=tenant',
		]);
	});
	it('refine posts the object and its natural key', async () => {
		const { api, fetchFn } = mk(ok(null, 204));
		await api.refine('revert', 'connector', { id: 'crm' });
		const [url, init] = fetchFn.mock.calls[0]!;
		expect(url).toBe('https://api.test/api/jobs/customer_care_agent/refinements/revert');
		expect(init.method).toBe('POST');
		expect(JSON.parse(init.body)).toEqual({ object: 'connector', key: { id: 'crm' } });
	});
	it('tenant and user routes are not job-scoped', async () => {
		const { api, fetchFn } = mk();
		await api.tenants();
		await api.tenantUsers('T1');
		await api.releaseConflicts('all');
		expect(fetchFn.mock.calls.map((c) => c[0])).toEqual([
			'https://api.test/api/tenants',
			'https://api.test/api/tenants/T1/users',
			'https://api.test/api/jobs/customer_care_agent/release-conflicts?status=all',
		]);
	});
	it('user writes name the tenant and the sub in the path', async () => {
		const { api, fetchFn } = mk(ok({}, 201));
		await api.saveTenantUser('T1', 'kc|42', { tenant_role: 'tenant_user', display_name: 'Ann' });
		expect(fetchFn.mock.calls[0]![0]).toBe('https://api.test/api/tenants/T1/users/kc%7C42');
		expect(JSON.parse(fetchFn.mock.calls[0]![1].body)).toEqual({
			tenant_role: 'tenant_user',
			display_name: 'Ann',
		});
	});
});
