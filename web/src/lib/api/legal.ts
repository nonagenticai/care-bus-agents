// Legal matters (plan 18 compliance, migration 0026, `/api/jobs/{job}/legal-matters`): the ethical wall around legal
// research. Types mirror crates/carebus-api src/routes/legal_matters.rs (MatterView, MemberView, StoreView). The
// server enforces every rule (RLS + routes); the helpers below only decide what the page offers.
//
// Who may do what: any tenant member creates a matter and becomes its lead. Members see the matter and its runs. A
// tenant_admin sees every matter (list, members, stores) and manages membership, but not for themselves, and does not
// see the matter's runs unless a lead made them a member. Lead or admin rename / close / delete and manage members;
// only a lead attaches stores.
import { keyMap } from './shapes';

export type MatterRole = 'lead' | 'member';
export const MATTER_ROLES: readonly MatterRole[] = ['lead', 'member'];
export const MATTER_ROLE_LABEL: Record<MatterRole, string> = { lead: 'Lead', member: 'Member' };
/** Server limits (routes::legal_matters). */
export const MATTER_NAME_MAX = 200;
export const MATTER_REFERENCE_MAX = 100;

/** routes::legal_matters `MatterView`: list rows, POST/PATCH answers, `matter` of GET one. */
export interface LegalMatter {
	id: string;
	name: string;
	reference: string | null;
	created_by: string;
	created_at: string;
	closed_at: string | null;
	/** The caller's role; null for a tenant_admin who is not a member. */
	my_role: MatterRole | null;
	member_count: number;
	store_count: number;
}

export interface MatterMember {
	user_sub: string;
	display_name: string | null;
	role: MatterRole;
	added_by: string;
	added_at: string;
}

export interface MatterStore {
	store_id: string;
	added_by: string;
	added_at: string;
}

/** GET /api/jobs/{job}/legal-matters/{id}. */
export interface LegalMatterDetail {
	matter: LegalMatter;
	members: MatterMember[];
	stores: MatterStore[];
}

export interface LegalMatterInput {
	name: string;
	reference?: string;
}

/** PATCH body: every key optional; `reference: ''` clears it; `closed` closes or reopens. */
export interface LegalMatterPatch {
	name?: string;
	reference?: string;
	closed?: boolean;
}

const MATTER = keyMap<LegalMatter>()({
	id: true,
	name: true,
	reference: true,
	created_by: true,
	created_at: true,
	closed_at: true,
	my_role: true,
	member_count: true,
	store_count: true,
});
const MEMBER = keyMap<MatterMember>()({
	user_sub: true,
	display_name: true,
	role: true,
	added_by: true,
	added_at: true,
});
const STORE = keyMap<MatterStore>()({ store_id: true, added_by: true, added_at: true });
const DETAIL = keyMap<LegalMatterDetail>()({ matter: true, members: true, stores: true });

/** Key maps the matter samples in tests/unit/legal-matters.test.ts are held to. */
export const LEGAL_SHAPES = { MATTER, MEMBER, STORE, DETAIL } as const;

// ---- what the page offers ----------------------------------------------------------------------------------------

/** Rename, close, reopen, delete, manage members: the lead, or a tenant_admin. */
export function canManage(m: Pick<LegalMatter, 'my_role'>, isAdmin: boolean): boolean {
	return isAdmin || m.my_role === 'lead';
}

/** Attach and detach stores: the lead only (not an admin). */
export function canEditStores(m: Pick<LegalMatter, 'my_role' | 'closed_at'>): boolean {
	return m.my_role === 'lead' && !m.closed_at;
}

/** Whether `sub` may be added or have their role changed by the caller: an admin who is not a lead may not touch
 * their own membership (the server refuses it). */
export function canGrant(m: Pick<LegalMatter, 'my_role'>, caller: string, sub: string): boolean {
	return m.my_role === 'lead' || sub.trim() !== caller;
}

/** Leads in the member list. */
export function leadCount(members: Pick<MatterMember, 'role'>[]): number {
	return members.filter((x) => x.role === 'lead').length;
}

/** Removing or demoting the only lead is refused (409 last_lead): the row's controls say so up front. */
export function isLastLead(members: Pick<MatterMember, 'role' | 'user_sub'>[], sub: string): boolean {
	return leadCount(members) === 1 && members.some((x) => x.user_sub === sub && x.role === 'lead');
}

/** `Name (reference)`, or the name alone. */
export function matterLabel(m: Pick<LegalMatter, 'name' | 'reference'>): string {
	return m.reference ? `${m.name} (${m.reference})` : m.name;
}

export interface MatterDraft {
	name: string;
	reference: string;
}

export function emptyMatterDraft(): MatterDraft {
	return { name: '', reference: '' };
}

/** Field → message, as the server would refuse it; `{}` = sendable. */
export function validateMatterDraft(d: MatterDraft): Partial<Record<keyof MatterDraft, string>> {
	const out: Partial<Record<keyof MatterDraft, string>> = {};
	const n = [...d.name.trim()].length;
	if (n === 0) out.name = 'Name the matter.';
	else if (n > MATTER_NAME_MAX) out.name = `At most ${MATTER_NAME_MAX} characters.`;
	if ([...d.reference.trim()].length > MATTER_REFERENCE_MAX)
		out.reference = `At most ${MATTER_REFERENCE_MAX} characters.`;
	return out;
}

/** The POST body: trimmed, the reference only when given. */
export function toMatterInput(d: MatterDraft): LegalMatterInput {
	const input: LegalMatterInput = { name: d.name.trim() };
	const r = d.reference.trim();
	if (r) input.reference = r;
	return input;
}

/** One line for the user from a matter refusal's code; `null` = show the server's message. */
export function matterProblem(code: string): string | null {
	switch (code) {
		case 'reference_taken':
			return 'Another matter of this job already has that reference.';
		case 'last_lead':
			return 'A matter needs at least one lead: make someone else lead first.';
		case 'matter_has_runs':
			return 'Research runs are filed under this matter; close it instead of deleting it.';
		case 'matter_has_stores':
			return 'Stores are attached to this matter; detach them before deleting it.';
		case 'store_in_other_matter':
			return 'That store (or one above or below it) already belongs to another matter.';
		case 'matter_closed':
			return 'This matter is closed. Reopen it first.';
		case 'unknown_user':
			return 'That person is not a user of this tenant.';
		case 'unknown_store':
			return 'That store is no longer a knowledge store of this job.';
		case 'not_a_tenant_user':
			return 'Only a user of this tenant can lead a matter.';
		default:
			return null;
	}
}
