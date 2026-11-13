// Legal matters (plan 18 compliance, migration 0026): the api module (shapes, helpers, request layer), the matters
// page (list, filters, create) and the matter drawer (settings, members panel, stores panel) for a lead, a plain
// member and a tenant_admin who is not a member.
import { cleanup, fireEvent, render, screen, within } from '@testing-library/svelte';
import { afterEach, describe, expect, it, vi } from 'vitest';
import { ApiError, createClient, type Api } from '../../src/lib/api/client';
import { app } from '../../src/lib/app.svelte';
import { session } from '../../src/lib/auth/session.svelte';
import {
	LEGAL_SHAPES,
	canEditStores,
	canGrant,
	canManage,
	emptyMatterDraft,
	isLastLead,
	matterLabel,
	matterProblem,
	toMatterInput,
	validateMatterDraft,
	type LegalMatter,
	type LegalMatterDetail,
} from '../../src/lib/api/legal';
import type { KnowledgeStoreItem } from '../../src/lib/api/types';
import { meAs } from './me';
import MattersPage from '../../src/routes/admin/jobs/[job]/matters/+page.svelte';
import MatterDetail from '../../src/routes/admin/jobs/[job]/matters/MatterDetail.svelte';

const ID = '0195f48b-d002-7000-8000-0000000000c1';
const STORE = '6f9619ff-8b86-d011-b42d-00cf4fc964ff';
const STORE2 = '7f9619ff-8b86-d011-b42d-00cf4fc964ff';

const matter = (over: Partial<LegalMatter> = {}): LegalMatter => ({
	id: ID,
	name: 'Kowalski v. Nowak',
	reference: 'I C 12/26',
	created_by: 'u1',
	created_at: '2026-10-23T09:00:00Z',
	closed_at: null,
	my_role: 'lead',
	member_count: 2,
	store_count: 1,
	...over,
});

const detail = (over: Partial<LegalMatter> = {}): LegalMatterDetail => ({
	matter: matter(over),
	members: [
		{
			user_sub: 'u1',
			display_name: 'A. Lead',
			role: 'lead',
			added_by: 'u1',
			added_at: '2026-10-23T09:00:00Z',
		},
		{ user_sub: 'u2', display_name: null, role: 'member', added_by: 'u1', added_at: '2026-10-23T09:01:00Z' },
	],
	stores: [{ store_id: STORE, added_by: 'u1', added_at: '2026-10-23T09:02:00Z' }],
});

const store = (key: string, label: string, sid: string | null): KnowledgeStoreItem =>
	({
		key,
		label,
		parent: null,
		kind: 'store',
		areas: [],
		description: null,
		strata_store_id: sid,
	}) as unknown as KnowledgeStoreItem;

const keys = (o: object) => Object.keys(o).sort();
const mapKeys = (m: Record<string, unknown>) =>
	Object.entries(m)
		.filter(([, v]) => v === true)
		.map(([k]) => k)
		.sort();

afterEach(() => {
	cleanup();
	app.api = null;
});

async function flush() {
	for (let i = 0; i < 5; i++) await Promise.resolve();
	await new Promise((r) => setTimeout(r, 0));
}

describe('legal matter shapes and helpers', () => {
	it('the samples carry exactly the keys routes::legal_matters serves', () => {
		const d = detail();
		expect(keys(d)).toEqual(mapKeys(LEGAL_SHAPES.DETAIL));
		expect(keys(d.matter)).toEqual(mapKeys(LEGAL_SHAPES.MATTER));
		for (const m of d.members) expect(keys(m)).toEqual(mapKeys(LEGAL_SHAPES.MEMBER));
		for (const s of d.stores) expect(keys(s)).toEqual(mapKeys(LEGAL_SHAPES.STORE));
	});

	it('who may manage, edit stores and grant', () => {
		expect(canManage({ my_role: 'lead' }, false)).toBe(true);
		expect(canManage({ my_role: 'member' }, false)).toBe(false);
		expect(canManage({ my_role: null }, true)).toBe(true);
		expect(canEditStores({ my_role: 'lead', closed_at: null })).toBe(true);
		expect(canEditStores({ my_role: 'lead', closed_at: '2026-10-23T10:00:00Z' })).toBe(false);
		// an admin is never a store editor unless lead
		expect(canEditStores({ my_role: null, closed_at: null })).toBe(false);
		// an admin who is not a lead cannot grant themselves
		expect(canGrant({ my_role: null }, 'adm', 'adm')).toBe(false);
		expect(canGrant({ my_role: null }, 'adm', 'u2')).toBe(true);
		expect(canGrant({ my_role: 'lead' }, 'u1', 'u1')).toBe(true);
	});

	it('the only lead is flagged; labels; refusal texts', () => {
		const d = detail();
		expect(isLastLead(d.members, 'u1')).toBe(true);
		expect(isLastLead(d.members, 'u2')).toBe(false);
		expect(isLastLead([...d.members, { ...d.members[1]!, user_sub: 'u3', role: 'lead' }], 'u1')).toBe(false);
		expect(matterLabel(matter())).toBe('Kowalski v. Nowak (I C 12/26)');
		expect(matterLabel(matter({ reference: null }))).toBe('Kowalski v. Nowak');
		expect(matterProblem('last_lead')).toMatch(/at least one lead/);
		expect(matterProblem('matter_has_stores')).toMatch(/detach them/);
		expect(matterProblem('store_in_other_matter')).toMatch(/another matter/);
		expect(matterProblem('nope')).toBeNull();
	});

	it('validates the draft as the server would and sends only what was given', () => {
		const d = emptyMatterDraft();
		expect(validateMatterDraft(d)).toEqual({ name: 'Name the matter.' });
		d.name = '  Spadek ';
		expect(validateMatterDraft(d)).toEqual({});
		expect(toMatterInput(d)).toEqual({ name: 'Spadek' });
		d.reference = ' II Ns 4/26 ';
		expect(toMatterInput(d)).toEqual({ name: 'Spadek', reference: 'II Ns 4/26' });
		d.reference = 'x'.repeat(101);
		expect(validateMatterDraft(d).reference).toBe('At most 100 characters.');
		d.name = 'x'.repeat(201);
		expect(validateMatterDraft(d).name).toBe('At most 200 characters.');
	});
});

describe('legal matters client', () => {
	it('sends each call to the job route with the right method and body', async () => {
		const fetchFn = vi.fn(async () => new Response('{}', { status: 200 }));
		const api = createClient({
			baseUrl: 'http://api',
			token: async () => 't',
			job: () => 'desk',
			fetch: fetchFn,
		});
		await api.legalMatters({ mine: true, open: true });
		await api.createLegalMatter({ name: 'n' });
		await api.legalMatter(ID);
		await api.updateLegalMatter(ID, { closed: true });
		await api.deleteLegalMatter(ID);
		await api.setMatterMember(ID, 'u 2', 'member');
		await api.removeMatterMember(ID, 'u2');
		await api.attachMatterStore(ID, STORE);
		await api.detachMatterStore(ID, STORE);
		const calls = fetchFn.mock.calls.map((c) => {
			const [url, init] = c as unknown as [string, RequestInit];
			return `${init.method} ${url}${init.body ? ` ${String(init.body)}` : ''}`;
		});
		const base = 'http://api/api/jobs/desk/legal-matters';
		expect(calls).toEqual([
			`GET ${base}?mine=true&open=true`,
			`POST ${base} {"name":"n"}`,
			`GET ${base}/${ID}`,
			`PATCH ${base}/${ID} {"closed":true}`,
			`DELETE ${base}/${ID}`,
			`PUT ${base}/${ID}/members/u%202 {"role":"member"}`,
			`DELETE ${base}/${ID}/members/u2`,
			`PUT ${base}/${ID}/stores/${STORE}`,
			`DELETE ${base}/${ID}/stores/${STORE}`,
		]);
	});
});

function fakeApi(over: Partial<Record<keyof Api, unknown>> = {}) {
	const base = {
		legalMatters: vi.fn(async () => ({ items: [matter()], next: null })),
		legalMatter: vi.fn(async () => detail()),
		createLegalMatter: vi.fn(async () =>
			matter({ id: 'new', name: 'Spadek', reference: null, member_count: 1 }),
		),
		updateLegalMatter: vi.fn(async () => matter()),
		deleteLegalMatter: vi.fn(async () => undefined),
		setMatterMember: vi.fn(async () => ({ user_sub: 'u3', role: 'member' })),
		removeMatterMember: vi.fn(async () => undefined),
		attachMatterStore: vi.fn(async () => ({ store_id: STORE2 })),
		detachMatterStore: vi.fn(async () => undefined),
		tenantUsers: vi.fn(async () => ({
			items: [
				{ sub: 'u1', tenant_role: 'tenant_user', display_name: 'A. Lead', updated_at: '', updated_by: '' },
				{ sub: 'u2', tenant_role: 'tenant_user', display_name: null, updated_at: '', updated_by: '' },
				{ sub: 'u3', tenant_role: 'tenant_user', display_name: 'C. Third', updated_at: '', updated_by: '' },
				{
					sub: 'adm',
					tenant_role: 'tenant_admin',
					display_name: 'The Admin',
					updated_at: '',
					updated_by: '',
				},
			],
			next: null,
		})),
		knowledgeStores: vi.fn(async () => ({
			items: [
				store('files', 'Client files', STORE),
				store('forms', 'Forms', STORE2),
				store('raw', 'Raw', null),
			],
			next: null,
		})),
	};
	const api = { ...base, ...over } as typeof base;
	app.api = api as unknown as Api;
	return api;
}

describe('/admin/jobs/[job]/matters', () => {
	it('lists matters with the caller role and sends the filters', async () => {
		session.set({ sub: 'adm', name: 'A' }, meAs('tenant_admin', 'adm'));
		const api = fakeApi({
			legalMatters: vi.fn(async () => ({ items: [matter({ my_role: null })], next: null })),
		});
		render(MattersPage);
		const table = await screen.findByRole('table', { name: 'Legal matters' });
		expect(within(table).getByText('Admin (no access to runs)')).toBeTruthy();
		await fireEvent.click(screen.getByLabelText(/Mine only/));
		await flush();
		expect(api.legalMatters).toHaveBeenLastCalledWith({ mine: true, open: undefined });
	});

	it('creates a matter and opens it; a taken reference is explained', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user', 'u1'));
		const api = fakeApi();
		render(MattersPage);
		await screen.findByRole('table', { name: 'Legal matters' });
		const form = screen.getByRole('form', { name: 'New legal matter' });
		await fireEvent.click(within(form).getByRole('button', { name: 'Create matter' }));
		expect(within(form).getByText('Name the matter.')).toBeTruthy();
		await fireEvent.input(within(form).getByLabelText(/^Name/), { target: { value: ' Spadek ' } });
		await fireEvent.click(within(form).getByRole('button', { name: 'Create matter' }));
		await flush();
		expect(api.createLegalMatter).toHaveBeenCalledWith({ name: 'Spadek' });
		expect(api.legalMatter).toHaveBeenCalledWith('new');
		api.createLegalMatter.mockRejectedValueOnce(new ApiError(409, 'reference_taken', 'taken'));
		await fireEvent.input(within(form).getByLabelText(/^Name/), { target: { value: 'Again' } });
		await fireEvent.click(within(form).getByRole('button', { name: 'Create matter' }));
		await flush();
		expect(within(form).getByText(/already has that reference.*\(reference_taken\)/)).toBeTruthy();
	});
});

describe('MatterDetail', () => {
	it('a lead manages members and stores; the only lead cannot be removed', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user', 'u1'));
		const api = fakeApi();
		render(MatterDetail, { id: ID, isAdmin: false });
		const members = await screen.findByRole('table', { name: 'Members' });
		expect((within(members).getByRole('button', { name: 'Remove u1' }) as HTMLButtonElement).disabled).toBe(
			true,
		);
		expect((within(members).getByRole('button', { name: 'Remove u2' }) as HTMLButtonElement).disabled).toBe(
			false,
		);
		await flush();
		// add u3 as a member: existing members are not offered
		const add = screen.getByRole('form', { name: 'Add member' });
		const person = within(add).getByLabelText('Person') as HTMLSelectElement;
		expect([...person.options].map((o) => o.value)).toEqual(['', 'u3', 'adm']);
		await fireEvent.change(person, { target: { value: 'u3' } });
		await fireEvent.click(within(add).getByRole('button', { name: 'Add member' }));
		await flush();
		expect(api.setMatterMember).toHaveBeenCalledWith(ID, 'u3', 'member');
		// stores: the attached one shows its label; only unattached indexed stores are offered
		expect(screen.getByRole('list', { name: 'Matter stores' }).textContent).toContain('Client files (files)');
		const attach = screen.getByRole('form', { name: 'Attach store' });
		const pick = within(attach).getByLabelText('Store') as HTMLSelectElement;
		expect([...pick.options].map((o) => o.value)).toEqual(['', STORE2]);
		await fireEvent.change(pick, { target: { value: STORE2 } });
		await fireEvent.click(within(attach).getByRole('button', { name: 'Attach store' }));
		await flush();
		expect(api.attachMatterStore).toHaveBeenCalledWith(ID, STORE2);
	});

	it('a tenant_admin who is not a member manages members but not themselves, and no stores', async () => {
		session.set({ sub: 'adm', name: 'A' }, meAs('tenant_admin', 'adm'));
		const api = fakeApi({ legalMatter: vi.fn(async () => detail({ my_role: null })) });
		render(MatterDetail, { id: ID, isAdmin: true });
		await screen.findByRole('table', { name: 'Members' });
		await flush();
		expect(screen.getByText('Not a member (no access to its research)')).toBeTruthy();
		const person = within(screen.getByRole('form', { name: 'Add member' })).getByLabelText(
			'Person',
		) as HTMLSelectElement;
		expect([...person.options].map((o) => o.value)).toEqual(['', 'u3']);
		expect(screen.queryByRole('form', { name: 'Attach store' })).toBeNull();
		expect(screen.queryByRole('button', { name: /^Detach/ })).toBeNull();
		expect(screen.getByText("Only the matter's lead attaches stores.")).toBeTruthy();
		// a refusal is shown in words
		api.removeMatterMember.mockRejectedValueOnce(new ApiError(409, 'last_lead', 'x'));
		await fireEvent.click(screen.getByRole('button', { name: 'Remove u2' }));
		await flush();
		expect(screen.getByText(/at least one lead.*\(last_lead\)/)).toBeTruthy();
	});

	it('a plain member sees the matter read-only', async () => {
		session.set({ sub: 'u2', name: 'B' }, meAs('tenant_user', 'u2'));
		fakeApi({ legalMatter: vi.fn(async () => detail({ my_role: 'member' })) });
		render(MatterDetail, { id: ID, isAdmin: false });
		await screen.findByRole('table', { name: 'Members' });
		expect(screen.queryByRole('form', { name: 'Add member' })).toBeNull();
		expect(screen.queryByRole('form', { name: 'Matter settings' })).toBeNull();
		expect(screen.queryByRole('button', { name: /^Remove/ })).toBeNull();
	});

	it('delete reports the matter gone; a matter with runs says close it instead', async () => {
		session.set({ sub: 'u1', name: 'A' }, meAs('tenant_user', 'u1'));
		const api = fakeApi();
		const changes: (LegalMatter | null)[] = [];
		render(MatterDetail, { id: ID, onchange: (_: string, m: LegalMatter | null) => changes.push(m) });
		await screen.findByRole('form', { name: 'Matter settings' });
		api.deleteLegalMatter.mockRejectedValueOnce(new ApiError(409, 'matter_has_runs', 'x'));
		await fireEvent.click(screen.getByRole('button', { name: 'Delete matter' }));
		await flush();
		expect(screen.getByText(/close it instead/)).toBeTruthy();
		await fireEvent.click(screen.getByRole('button', { name: 'Delete matter' }));
		await flush();
		expect(changes).toEqual([null]);
	});
});
