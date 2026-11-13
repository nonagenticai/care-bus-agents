// `/api/providers`: the tenant's model providers, in two parts (spec 2026-10-18-demo-providers-laya-gates §2).
//
// 1. LLM providers (`GET/POST /api/providers`, `PUT/DELETE /api/providers/{id}`, `POST /api/providers/{id}/test`,
//    `PUT /api/providers/uses/{use}`): carebus-api's own registry of OpenAI-compatible and Laya endpoints, and which
//    one serves `qualify` (request qualification), `gates` (Laya System-1 gates), and plan 18's `research` (the
//    researcher's plan/read/act loop) and `research_write` (its memo writer). `token` is write-only: no response
//    carries it, only `has_token`.
// 2. Slots (`GET /api/providers/slots`, `PUT/DELETE /api/providers/slots/{capability}`): strata_rag (embed, rerank,
//    chat) and strata_ingest (ocr, vision, transcribe) registrations, proxied by carebus-api. `api_key` is
//    write-only, only `has_api_key` comes back.
//
// Types mirror crates/carebus-api src/routes/providers.rs; the fixtures in src/lib/api/fixtures/providers/ are
// checked against LLM_SHAPES and PROVIDER_SHAPES (tests/unit/providers.test.ts).
import { keyMap } from './shapes';

// ---- LLM providers ----------------------------------------------------------------------------------------------

export type LlmKind = 'openai' | 'laya';
export type LlmUse = 'qualify' | 'gates' | 'research' | 'research_write';
export const LLM_KINDS: readonly LlmKind[] = ['openai', 'laya'];
export const LLM_USES: readonly LlmUse[] = ['qualify', 'gates', 'research', 'research_write'];

export interface LlmProvider {
	id: string;
	name: string;
	kind: LlmKind;
	base_url: string;
	has_token: boolean;
	updated_at: string;
	updated_by: string | null;
}

/** A use's binding; `provider_id: null` = unbound (qualify falls back to the platform default, gates are off). */
export interface LlmUseBinding {
	use: LlmUse;
	provider_id: string | null;
	model: string | null;
}

/** The deployment's own LLM (env), used for `qualify` when the tenant binds none. Where it runs is shown. */
export interface PlatformDefault {
	base_url: string | null;
	model: string | null;
	configured: boolean;
}

export interface LlmProviders {
	providers: LlmProvider[];
	uses: LlmUseBinding[];
	platform_default: PlatformDefault;
	/**
	 * May a research run use a model other than the platform's own (migration 0023, default false)? Research runs
	 * only: qualify and gates ignore it. Off, a research run bound to an external provider fails, never falls back.
	 */
	external_llm_allowed: boolean;
}

/** PUT /api/providers/settings (tenant_admin); the answer echoes the stored value. */
export interface LlmSettings {
	external_llm_allowed: boolean;
}

/** POST /api/providers and PUT /api/providers/{id}: `token` absent = keep the stored one, `""` = clear it. */
export interface LlmProviderInput {
	name: string;
	kind: LlmKind;
	base_url: string;
	token?: string;
}

/** POST /api/providers/{id}/test: `GET {base_url}/models` (openai) or `{base_url}/v1/models` (laya). */
export interface LlmProviderTest {
	ok: boolean;
	status: number | null;
	latency_ms: number;
	models: string[];
}

/** PUT /api/providers/uses/{use}. */
export interface LlmUseInput {
	provider_id: string | null;
	model: string | null;
}

const LLM_PROVIDER = keyMap<LlmProvider>()({
	id: true,
	name: true,
	kind: true,
	base_url: true,
	has_token: true,
	updated_at: true,
	updated_by: true,
});
const LLM_USE = keyMap<LlmUseBinding>()({ use: true, provider_id: true, model: true });
const PLATFORM_DEFAULT = keyMap<PlatformDefault>()({ base_url: true, model: true, configured: true });
const LLM_ROOT = keyMap<LlmProviders>()({
	providers: true,
	uses: true,
	platform_default: true,
	external_llm_allowed: true,
});
const LLM_TEST = keyMap<LlmProviderTest>()({ ok: true, status: true, latency_ms: true, models: true });

/** Key maps the LLM provider fixtures are held to (tests/unit/providers.test.ts). */
export const LLM_SHAPES = {
	ROOT: LLM_ROOT,
	PROVIDER: LLM_PROVIDER,
	USE: LLM_USE,
	PLATFORM_DEFAULT,
	TEST: LLM_TEST,
} as const;

/**
 * What runs when a use is unbound: `platform` = the deployment's platform default (`noDefault` says what happens
 * without one), `off` = nothing runs, `{ use }` = that other use's binding serves it.
 */
export type UseFallback =
	| { kind: 'platform'; who: string; noDefault: { summary: string; notice: string } }
	| { kind: 'off'; summary: string; notice: string }
	| { kind: 'use'; use: LlmUse };

export interface UseRule {
	label: string;
	/** The only provider kind it takes (carebus-api refuses any other). */
	kind: LlmKind;
	purpose: string;
	/** The provider form's "Use for" checkbox. */
	useFor: string;
	/** The uses form's "no provider" option. */
	none: string;
	fallback: UseFallback;
}

/** What each use is for, the provider kind it needs, and what runs while it is unbound. */
export const USE_RULES: Record<LlmUse, UseRule> = {
	qualify: {
		label: 'Qualify',
		kind: 'openai',
		purpose: 'Reads each customer message and decides the request type before it goes on the bus.',
		useFor: 'Use for chat qualification (qualify)',
		none: 'None (platform default)',
		fallback: {
			kind: 'platform',
			who: 'chat uses',
			noDefault: {
				summary: 'No LLM — keyword rules',
				notice: 'requests are qualified by keyword rules, no LLM',
			},
		},
	},
	gates: {
		label: 'Laya gates',
		kind: 'laya',
		purpose: 'Answers the gate questions at request, tool and response points (Laya System-1).',
		useFor: 'Use for Laya gates (gates)',
		none: 'None (gates off)',
		fallback: {
			kind: 'off',
			summary: 'Laya gates off',
			notice: 'Laya gates are off, no gate question runs',
		},
	},
	research: {
		label: 'Research (plan/read/act)',
		kind: 'openai',
		purpose: 'Drives a research run: plans, calls the search and source tools, and reads what they return.',
		useFor: 'Use for research runs (research)',
		none: 'None (platform default)',
		fallback: {
			kind: 'platform',
			who: 'research runs use',
			noDefault: {
				summary: 'No LLM — research runs fail',
				notice: 'no platform default is configured, research runs cannot start',
			},
		},
	},
	research_write: {
		label: 'Research memo writer',
		kind: 'openai',
		purpose:
			'Writes the final memo of a research run from its findings. Citations are checked by code, not a model.',
		useFor: 'Use for research memos (research_write)',
		none: 'None (same as Research)',
		fallback: { kind: 'use', use: 'research' },
	},
};

export const KIND_LABELS: Record<LlmKind, string> = { openai: 'OpenAI-compatible', laya: 'Laya' };

/** The binding for `use`; an absent row reads as unbound. */
export function bindingOf(d: LlmProviders, use: LlmUse): LlmUseBinding {
	return d.uses.find((u) => u.use === use) ?? { use, provider_id: null, model: null };
}

/** The providers a use can be bound to (its kind only). */
export function providersFor(d: LlmProviders, use: LlmUse): LlmProvider[] {
	return d.providers.filter((p) => p.kind === USE_RULES[use].kind);
}

/** Uses that name `id`: a provider in use cannot be deleted (409 provider_in_use). */
export function usesOf(d: LlmProviders, id: string): LlmUse[] {
	return d.uses.filter((u) => u.provider_id === id).map((u) => u.use);
}

const platformText = (pd: PlatformDefault) =>
	`${pd.model ?? '(no model named)'} @ ${pd.base_url ?? '(no base URL)'}`;

/**
 * One line on what serves a use right now. An unbound use says its fallback (USE_RULES): the platform default when
 * the deployment configures one, nothing (gates), or the use it falls back to (research_write → research).
 */
export function useSummary(d: LlmProviders, use: LlmUse): { word: 'ok' | 'idle' | 'warn'; text: string } {
	const b = bindingOf(d, use);
	const p = b.provider_id ? d.providers.find((x) => x.id === b.provider_id) : undefined;
	if (b.provider_id)
		return {
			word: 'ok',
			text: `${p?.name ?? b.provider_id}${b.model ? ` · ${b.model}` : ''}`,
		};
	const fb = USE_RULES[use].fallback;
	if (fb.kind === 'off') return { word: 'warn', text: fb.summary };
	if (fb.kind === 'use') {
		const other = useSummary(d, fb.use);
		return {
			word: other.word === 'ok' ? 'idle' : other.word,
			text: `Same as ${USE_RULES[fb.use].label}: ${other.text}`,
		};
	}
	const pd = d.platform_default;
	if (pd.configured) return { word: 'idle', text: `Platform default: ${platformText(pd)}` };
	return { word: 'warn', text: fb.noDefault.summary };
}

/** The add/edit form. `token` starts blank: a stored token is never shown. `clearToken` sends `""`. */
export interface LlmDraft {
	name: string;
	kind: LlmKind;
	base_url: string;
	token: string;
	clearToken: boolean;
}

export function llmDraftFor(p: LlmProvider | null): LlmDraft {
	return p
		? { name: p.name, kind: p.kind, base_url: p.base_url, token: '', clearToken: false }
		: { name: '', kind: 'openai', base_url: '', token: '', clearToken: false };
}

/** Host-name suffixes carebus-api refuses (crate::netguard `INTERNAL_SUFFIXES`): cluster, tailnet and LAN names. */
const INTERNAL_SUFFIXES = [
	'.internal',
	'.svc',
	'.cluster.local',
	'.local',
	'.localhost',
	'.lan',
	'.home.arpa',
];

/** netguard `internal_hostname`: dotless names and names under an internal suffix (or the suffix itself). */
function internalHostname(host: string): boolean {
	const h = host.replace(/\.+$/u, '').toLowerCase();
	return !h.includes('.') || INTERNAL_SUFFIXES.some((s) => h.endsWith(s) || h === s.slice(1));
}

/** netguard `public_v4`. */
function publicV4(a: number, b: number, c: number, d: number): boolean {
	return !(
		(a === 0 && b === 0 && c === 0 && d === 0) ||
		a === 127 ||
		a === 10 ||
		(a === 172 && b >= 16 && b <= 31) ||
		(a === 192 && b === 168) ||
		(a === 169 && b === 254) ||
		(a === 255 && b === 255 && c === 255 && d === 255) ||
		(a >= 224 && a <= 239) ||
		(a === 192 && b === 0 && c === 2) ||
		(a === 198 && b === 51 && c === 100) ||
		(a === 203 && b === 0 && c === 113) ||
		a === 0 ||
		(a === 100 && b >= 64 && b <= 127) ||
		(a === 192 && b === 0 && c === 0) ||
		(a === 198 && (b === 18 || b === 19)) ||
		a >= 240
	);
}

/** The 8 segments of an IPv6 literal as the URL parser serialises it (hex pieces, at most one `::`). */
function v6Segments(h: string): number[] | null {
	const parts = h.split('::');
	if (parts.length > 2) return null;
	const pieces = (p: string) =>
		p ? p.split(':').map((x) => (/^[0-9a-f]{1,4}$/iu.test(x) ? parseInt(x, 16) : NaN)) : [];
	const head = pieces(parts[0] ?? '');
	const tail = parts.length === 2 ? pieces(parts[1] ?? '') : [];
	const fill = parts.length === 2 ? 8 - head.length - tail.length : 0;
	if (fill < 0) return null;
	const segs = [...head, ...new Array<number>(fill).fill(0), ...tail];
	return segs.length === 8 && segs.every((x) => !Number.isNaN(x)) ? segs : null;
}

/** netguard `public_v6`. */
function publicV6(segs: number[]): boolean {
	const [s0 = 0, s1 = 0, , , , s5 = 0, s6 = 0, s7 = 0] = segs;
	const v4 = (hi: number, lo: number) => publicV4(hi >> 8, hi & 0xff, lo >> 8, lo & 0xff);
	const zero = (from: number, to: number) => segs.slice(from, to).every((x) => x === 0);
	if (zero(0, 5) && s5 === 0xffff) return v4(s6, s7); // ::ffff:a.b.c.d
	if (s0 === 0x0064 && s1 === 0xff9b && zero(2, 6)) return v4(s6, s7); // 64:ff9b::/96 NAT64
	return !(
		zero(0, 8) ||
		(zero(0, 7) && s7 === 1) ||
		(s0 & 0xff00) === 0xff00 ||
		(s0 & 0xfe00) === 0xfc00 ||
		(s0 & 0xffc0) === 0xfe80 ||
		(s0 === 0x2001 && s1 === 0x0db8) ||
		s0 === 0x2002 ||
		(s0 === 0x2001 && s1 === 0) ||
		zero(0, 6)
	);
}

/** netguard `check_url`'s host rule on a parsed hostname: '' when allowed, else the problem. */
function hostProblem(hostname: string): string {
	const PRIVATE =
		'Use a public host: private, loopback, link-local and tailnet (CGNAT) addresses are refused.';
	if (hostname.startsWith('[')) {
		const segs = v6Segments(hostname.slice(1, -1));
		return segs && publicV6(segs) ? '' : PRIVATE;
	}
	const m = /^(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})$/u.exec(hostname);
	if (m) {
		const [a = 0, b = 0, c = 0, d = 0] = m.slice(1).map(Number);
		return publicV4(a, b, c, d) ? '' : PRIVATE;
	}
	if (internalHostname(hostname))
		return 'Use a public host name: localhost, dotless and *.internal, *.svc, *.cluster.local, *.local, *.lan names are refused.';
	return '';
}

/**
 * carebus-api's `base_url` rule: http or https, a host, no userinfo, query or fragment, and a public host (the same
 * rule as crate::netguard: no non-public literal IP, no localhost, dotless or internal name). Returns the problem,
 * or '' when the URL is acceptable. The server also checks what the name resolves to; that it cannot see here.
 */
export function baseUrlProblem(raw: string): string {
	const s = raw.trim();
	if (!s) return 'Enter the base URL.';
	if (/\s/u.test(s)) return 'No spaces in the URL.';
	let u: URL;
	try {
		u = new URL(s);
	} catch {
		return 'Not a URL; it starts with http:// or https://.';
	}
	if (u.protocol !== 'http:' && u.protocol !== 'https:') return 'Use an http or https URL.';
	if (!u.hostname) return 'The URL needs a host.';
	if (u.username || u.password || s.includes('@')) return 'No user or password in the URL; use the token.';
	if (u.search || s.includes('?')) return 'No query string.';
	if (u.hash || s.includes('#')) return 'No fragment.';
	return hostProblem(u.hostname);
}

/** Field → message; empty = nothing found. The server stays the authority. */
export function validateLlmDraft(d: LlmDraft): Partial<Record<'name' | 'kind' | 'base_url', string>> {
	const e: Partial<Record<'name' | 'kind' | 'base_url', string>> = {};
	if (!d.name.trim()) e.name = 'Enter a name.';
	if (!LLM_KINDS.includes(d.kind)) e.kind = 'Use openai or laya.';
	const url = baseUrlProblem(d.base_url);
	if (url) e.base_url = url;
	return e;
}

/** The POST/PUT body: trimmed; `token` only when typed (set) or cleared (`""`), absent otherwise (keep). */
export function toLlmInput(d: LlmDraft): LlmProviderInput {
	const input: LlmProviderInput = { name: d.name.trim(), kind: d.kind, base_url: d.base_url.trim() };
	const t = d.token.trim();
	if (d.clearToken) input.token = '';
	else if (t) input.token = t;
	return input;
}

/** Model choices for a use's picker: the last Test's list, with the bound model kept even if it is not listed. */
export function modelChoices(tested: LlmProviderTest | undefined, current: string | null): string[] | null {
	if (!tested || tested.models.length === 0) return null;
	const out = [...tested.models];
	if (current && !out.includes(current)) out.unshift(current);
	return out;
}

/** "ok · HTTP 200 · 84 ms" for a test result. */
export function testLine(t: LlmProviderTest): string {
	return `${t.ok ? 'ok' : 'failed'} · ${t.status == null ? 'no answer' : `HTTP ${t.status}`} · ${t.latency_ms} ms`;
}

// ---- in-form test and "Use for" --------------------------------------------------------------------------------

/**
 * POST /api/providers/test: tests the form's values without saving. `token` when one is typed; else, when editing,
 * `provider_id` so the server uses the stored token (only for the stored base_url: a new one answers 422
 * `token_required_for_new_base_url`).
 */
export interface LlmProviderTestInput {
	kind: LlmKind;
	base_url: string;
	token?: string;
	provider_id?: string;
}

/** The test body for the form: a typed token wins; Clear token tests without one; blank on edit = the stored one. */
export function toTestInput(d: LlmDraft, editingId: string | null): LlmProviderTestInput {
	const input: LlmProviderTestInput = { kind: d.kind, base_url: d.base_url.trim() };
	const t = d.token.trim();
	if (d.clearToken) return input;
	if (t) input.token = t;
	else if (editingId) input.provider_id = editingId;
	return input;
}

/** The form's Test error as one line for the user; `null` = not one this form explains itself. */
export function draftTestProblem(code: string): string | null {
	return code === 'token_required_for_new_base_url'
		? 'Enter the token again for a new URL: the stored token is never sent to another host.'
		: null;
}

/** The uses a provider of `kind` can serve (qualify, research, research_write ← openai; gates ← laya). */
export function usesForKind(kind: LlmKind): LlmUse[] {
	return LLM_USES.filter((u) => USE_RULES[u].kind === kind);
}

/** Laya model names offered while the form has no Test result to pick from. */
export const LAYA_MODEL_SUGGESTIONS: readonly string[] = ['jev-latest', 'laya-multilingual', 'laya-english'];

/** The form's "Use for" choice, per use. */
export interface UseForChoice {
	on: boolean;
	model: string;
}

/** "Use for" as the form opens: checked (with its model) where this provider is already bound. */
export function useForInitial(
	d: LlmProviders | null,
	editingId: string | null,
): Record<LlmUse, UseForChoice> {
	const out = {} as Record<LlmUse, UseForChoice>;
	for (const u of LLM_USES) {
		const b = d ? bindingOf(d, u) : null;
		const here = !!editingId && b?.provider_id === editingId;
		out[u] = { on: here, model: here ? (b?.model ?? '') : '' };
	}
	return out;
}

/**
 * The PUT /api/providers/uses/{use} body to send after the provider `savedId` was saved, or `null` when the binding
 * already says so. Checked binds it (with the model, blank = none); unchecking a binding of this provider unbinds
 * the use; unchecked and bound elsewhere leaves that binding alone.
 */
export function useForPlan(
	d: LlmProviders | null,
	editingId: string | null,
	use: LlmUse,
	choice: UseForChoice,
	savedId: string,
): LlmUseInput | null {
	const b = d ? bindingOf(d, use) : null;
	const model = choice.model.trim() || null;
	const here = !!editingId && b?.provider_id === editingId;
	if (choice.on) return here && (b?.model ?? null) === model ? null : { provider_id: savedId, model };
	return here ? { provider_id: null, model: null } : null;
}

/**
 * The form's model picker: the in-form Test's models (keeping the chosen one), else `null` = free text. For a free
 * text Laya model, `suggestions` names the known Laya models.
 */
export function formModelChoices(
	kind: LlmKind,
	tested: LlmProviderTest | undefined,
	current: string | null,
): { choices: string[] | null; suggestions: readonly string[] } {
	return {
		choices: modelChoices(tested, current),
		suggestions: kind === 'laya' ? LAYA_MODEL_SUGGESTIONS : [],
	};
}

/**
 * What an unbound use means, said loudly: `null` when bound. A platform fallback names the default it runs on (or
 * what happens without one); unbound gates means no gate question runs; research_write names the use it borrows.
 */
export function unboundNotice(d: LlmProviders, use: LlmUse): string | null {
	if (bindingOf(d, use).provider_id) return null;
	const fb = USE_RULES[use].fallback;
	if (fb.kind === 'off') return `${use}: not bound — ${fb.notice}`;
	if (fb.kind === 'use') {
		const other = useSummary(d, fb.use);
		return `${use}: not bound — the ${fb.use} binding writes the memo (${other.text})`;
	}
	const pd = d.platform_default;
	if (pd.configured) return `${use}: not bound — ${fb.who} the platform default (${platformText(pd)})`;
	return `${use}: not bound — ${fb.noDefault.notice}`;
}

// ---- slots (strata_rag / strata_ingest) -------------------------------------------------------------------------

export type ProviderOwner = 'strata_rag' | 'strata_ingest';
/** As the owner reports it; `null` when the owner could not be asked. */
export type ProviderSource = 'tenant' | 'default' | 'unconfigured';
export type ProviderServiceStatus = 'ok' | 'unavailable' | 'unconfigured';

/**
 * A tenant registration. `dims` is strata_rag's (embed); `thinking` / `temperature` its chat's (`null` = not sent to
 * the provider). Each is `null` for every other capability.
 */
export interface ProviderView {
	base_url: string;
	dialect: string;
	model: string;
	model_id: string | null;
	dims: number | null;
	regions: string[];
	zdr: boolean;
	timeout_ms: number;
	concurrency: number;
	has_api_key: boolean;
	thinking: boolean | null;
	temperature: number | null;
	updated_at: string | null;
}

/** A deployment default: what it is, never where it runs. A default rerank has `dims: null`. */
export interface ProviderDefault {
	dialect: string;
	model: string;
	dims: number | null;
}

export interface ProviderCapability {
	capability: string;
	owner: ProviderOwner;
	/** The owner answered. `false` = it is down or not configured, and `source` is `null`. */
	available: boolean;
	source: ProviderSource | null;
	provider: ProviderView | null;
	default: ProviderDefault | null;
}

export interface ProviderService {
	name: ProviderOwner;
	status: ProviderServiceStatus;
}

export interface ProviderSlots {
	services: ProviderService[];
	capabilities: ProviderCapability[];
}

/**
 * PUT /api/providers/slots/{capability}. `dims` for embed only; `thinking` / `temperature` for chat only (`null` = not
 * sent); `api_key` omitted = keep the stored one.
 */
export interface ProviderInput {
	base_url: string;
	dialect: string;
	model: string;
	model_id: string | null;
	dims?: number | null;
	regions: string[];
	zdr: boolean;
	timeout_ms: number;
	concurrency: number;
	thinking?: boolean | null;
	temperature?: number | null;
	api_key?: string;
}

export interface ProviderErrorBody {
	error: { code: string; message: string };
}

const VIEW = keyMap<ProviderView>()({
	base_url: true,
	dialect: true,
	model: true,
	model_id: true,
	dims: true,
	regions: true,
	zdr: true,
	timeout_ms: true,
	concurrency: true,
	has_api_key: true,
	thinking: true,
	temperature: true,
	updated_at: true,
});
const DEFAULT = keyMap<ProviderDefault>()({ dialect: true, model: true, dims: true });
const CAPABILITY = keyMap<ProviderCapability>()({
	capability: true,
	owner: true,
	available: true,
	source: true,
	provider: true,
	default: true,
});
const SERVICE = keyMap<ProviderService>()({ name: true, status: true });
const ROOT = keyMap<ProviderSlots>()({ services: true, capabilities: true });

/** Key maps the slots fixture (providers/providers.json) is held to (tests/unit/providers.test.ts). */
export const PROVIDER_SHAPES = { ROOT, SERVICE, CAPABILITY, VIEW, DEFAULT } as const;
export const PROVIDER_VALUES = {
	owner: ['strata_rag', 'strata_ingest'],
	source: ['tenant', 'default', 'unconfigured', null],
	status: ['ok', 'unavailable', 'unconfigured'],
} as const;

// ---- per-capability rules ---------------------------------------------------------------------------------------

export interface CapabilityRule {
	label: string;
	/** What it is used for, one line. */
	purpose: string;
	/** Dialects the owner implements for it. */
	dialects: string[];
	/** Upper bound for `timeout_ms`. */
	maxTimeoutMs: number;
	maxConcurrency: number;
	defaultTimeoutMs: number;
	defaultConcurrency: number;
	/** `dims` is sent and must be this value (embed only). */
	dims: number | null;
	/** `thinking` and `temperature` are offered and sent (chat only). */
	chatSettings: boolean;
}

/** strata_rag's bounds for a chat `temperature` (`providers::validate`). */
export const TEMPERATURE_RANGE = { min: 0, max: 2 } as const;

/**
 * strata_rag (#14, #16): one implemented dialect per capability (embed openai, rerank cohere, chat openai), tenant
 * timeout at most 30000 (embed) / 15000 (rerank) / 60000 (chat), dims 1024 (embed only), chat temperature 0..=2.
 * strata_ingest README (d0a439b): ocr mistral, vision and transcribe openai; timeout at most 300000 (ocr,
 * transcribe) / 110000 (vision), default 60000; concurrency 1..=64, default 4. The services stay the authority.
 */
export const CAPABILITY_RULES: Record<string, CapabilityRule> = {
	embed: {
		label: 'Embeddings',
		purpose: 'Turns text into vectors for knowledge search (strata_rag).',
		dialects: ['openai'],
		maxTimeoutMs: 30_000,
		maxConcurrency: 256,
		defaultTimeoutMs: 30_000,
		defaultConcurrency: 8,
		dims: 1024,
		chatSettings: false,
	},
	rerank: {
		label: 'Reranking',
		purpose: 'Orders search hits by relevance (strata_rag).',
		dialects: ['cohere'],
		maxTimeoutMs: 15_000,
		maxConcurrency: 256,
		defaultTimeoutMs: 15_000,
		defaultConcurrency: 8,
		dims: null,
		chatSettings: false,
	},
	chat: {
		label: 'Chat',
		purpose: 'Writes answers and expands search queries (strata_rag).',
		dialects: ['openai'],
		maxTimeoutMs: 60_000,
		maxConcurrency: 256,
		defaultTimeoutMs: 60_000,
		defaultConcurrency: 8,
		dims: null,
		chatSettings: true,
	},
	ocr: {
		label: 'OCR',
		purpose: 'Reads scanned pages during ingestion (strata_ingest).',
		dialects: ['mistral'],
		maxTimeoutMs: 300_000,
		maxConcurrency: 64,
		defaultTimeoutMs: 60_000,
		defaultConcurrency: 4,
		dims: null,
		chatSettings: false,
	},
	vision: {
		label: 'Vision',
		purpose: 'Captions images during ingestion (strata_ingest).',
		dialects: ['openai'],
		maxTimeoutMs: 110_000,
		maxConcurrency: 64,
		defaultTimeoutMs: 60_000,
		defaultConcurrency: 4,
		dims: null,
		chatSettings: false,
	},
	transcribe: {
		label: 'Transcription',
		purpose: 'Transcribes audio and video during ingestion (strata_ingest).',
		dialects: ['openai'],
		maxTimeoutMs: 300_000,
		maxConcurrency: 64,
		defaultTimeoutMs: 60_000,
		defaultConcurrency: 4,
		dims: null,
		chatSettings: false,
	},
};

/** A capability the page has no rule for yet: the services' own bounds. */
export function ruleFor(capability: string, owner: ProviderOwner): CapabilityRule {
	return (
		CAPABILITY_RULES[capability] ?? {
			label: capability,
			purpose: `Offered by ${owner}.`,
			dialects: ['openai'],
			maxTimeoutMs: owner === 'strata_rag' ? 30_000 : 300_000,
			maxConcurrency: owner === 'strata_rag' ? 256 : 64,
			defaultTimeoutMs: owner === 'strata_rag' ? 30_000 : 60_000,
			defaultConcurrency: owner === 'strata_rag' ? 8 : 4,
			dims: null,
			chatSettings: false,
		}
	);
}

export const PROBE_CAVEAT =
	'The provider must be reachable over public https; registration makes a live test call.';

// ---- form -------------------------------------------------------------------------------------------------------

/**
 * The editable form. `api_key` is always blank to begin with: the stored key is never shown. `thinking` and
 * `temperature` (chat) stay `null` until set: `null` is sent as "not sent to the provider".
 */
export interface ProviderDraft {
	base_url: string;
	dialect: string;
	model: string;
	model_id: string;
	dims: number | null;
	regions: string;
	zdr: boolean;
	timeout_ms: number;
	concurrency: number;
	thinking: boolean | null;
	temperature: number | null;
	api_key: string;
}

export function draftFor(c: ProviderCapability): ProviderDraft {
	const r = ruleFor(c.capability, c.owner);
	const p = c.provider;
	if (p) {
		return {
			base_url: p.base_url,
			dialect: p.dialect,
			model: p.model,
			model_id: p.model_id ?? '',
			dims: r.dims === null ? null : (p.dims ?? r.dims),
			regions: p.regions.join(', '),
			zdr: p.zdr,
			timeout_ms: p.timeout_ms,
			concurrency: p.concurrency,
			thinking: r.chatSettings ? (p.thinking ?? null) : null,
			temperature: r.chatSettings ? (p.temperature ?? null) : null,
			api_key: '',
		};
	}
	return {
		base_url: '',
		dialect:
			c.default?.dialect && r.dialects.includes(c.default.dialect) ? c.default.dialect : r.dialects[0]!,
		model: '',
		model_id: '',
		dims: r.dims,
		regions: 'eu',
		zdr: true,
		timeout_ms: r.defaultTimeoutMs,
		concurrency: r.defaultConcurrency,
		thinking: null,
		temperature: null,
		api_key: '',
	};
}

/** Whether saving needs a key: nothing stored, or a new base_url (a stored key never goes to a new host). */
export function keyRequired(c: ProviderCapability, d: ProviderDraft): boolean {
	const p = c.provider;
	return !p || !p.has_api_key || p.base_url !== d.base_url.trim();
}

/** A number input that was emptied reads as `null`, `undefined` or NaN: all of them mean "not set". */
const setTemperature = (t: number | null | undefined): number | null =>
	typeof t === 'number' && !Number.isNaN(t) ? t : null;

const parseRegions = (s: string) =>
	s
		.split(',')
		.map((x) => x.trim().toLowerCase())
		.filter(Boolean);

/** Fails fast on what the owner would refuse anyway. Field name → message; empty = nothing found. */
export function validateDraft(c: ProviderCapability, d: ProviderDraft): Record<string, string> {
	const r = ruleFor(c.capability, c.owner);
	const e: Record<string, string> = {};
	const url = d.base_url.trim();
	if (!url) e.base_url = 'Enter the provider base URL.';
	else if (!/^https:\/\/[^\s/?#]+[^\s?#]*$/u.test(url))
		e.base_url = 'Must be an https URL without query, fragment or spaces.';
	if (!d.model.trim()) e.model = 'Enter the model name.';
	if (!r.dialects.includes(d.dialect)) e.dialect = `Use ${r.dialects.join(' or ')}.`;
	if (r.dims !== null && d.dims !== r.dims) e.dims = `Must be ${r.dims}.`;
	if (parseRegions(d.regions).length === 0) e.regions = 'Name at least one region, for example eu.';
	if (!Number.isInteger(d.timeout_ms) || d.timeout_ms < 1 || d.timeout_ms > r.maxTimeoutMs)
		e.timeout_ms = `1 to ${r.maxTimeoutMs} ms.`;
	if (!Number.isInteger(d.concurrency) || d.concurrency < 1 || d.concurrency > r.maxConcurrency)
		e.concurrency = `1 to ${r.maxConcurrency}.`;
	const t = setTemperature(d.temperature);
	if (r.chatSettings && t !== null && (t < TEMPERATURE_RANGE.min || t > TEMPERATURE_RANGE.max))
		e.temperature = `${TEMPERATURE_RANGE.min} to ${TEMPERATURE_RANGE.max}.`;
	if (keyRequired(c, d) && !d.api_key.trim())
		e.api_key = c.provider?.has_api_key
			? 'A new base URL needs its own key; the stored key is never sent to another host.'
			: 'Enter the API key.';
	return e;
}

/**
 * The PUT body: trimmed values, `dims` for embed only, `thinking` / `temperature` for chat only (`null` when not set),
 * `api_key` only when one was typed.
 */
export function toProviderInput(c: ProviderCapability, d: ProviderDraft): ProviderInput {
	const r = ruleFor(c.capability, c.owner);
	const input: ProviderInput = {
		base_url: d.base_url.trim(),
		dialect: d.dialect,
		model: d.model.trim(),
		model_id: d.model_id.trim() || null,
		regions: parseRegions(d.regions),
		zdr: d.zdr,
		timeout_ms: d.timeout_ms,
		concurrency: d.concurrency,
	};
	if (r.dims !== null) input.dims = d.dims;
	if (r.chatSettings) {
		input.thinking = d.thinking ?? null;
		input.temperature = setTemperature(d.temperature);
	}
	const key = d.api_key.trim();
	if (key) input.api_key = key;
	return input;
}

/** Which form field an owner's error code is about, so the message is shown next to it. */
export function errorField(code: string): keyof ProviderDraft | null {
	switch (code) {
		case 'region_not_allowed':
			return 'regions';
		case 'zdr_required':
			return 'zdr';
		case 'dims_must_be_1024':
			return 'dims';
		case 'dialect_not_allowed':
			return 'dialect';
		case 'provider_host_not_allowed':
			return 'base_url';
		case 'api_key_required':
			return 'api_key';
		default:
			return null;
	}
}

/** Table cell for the source column. */
export function sourceLabel(c: ProviderCapability): { word: string; label: string } {
	if (!c.available) return { word: 'unavailable', label: `${c.owner} unavailable` };
	switch (c.source) {
		case 'tenant':
			return { word: 'ok', label: 'Tenant provider' };
		case 'default':
			return { word: 'idle', label: 'Deployment default' };
		default:
			return { word: 'warn', label: 'Not configured' };
	}
}
