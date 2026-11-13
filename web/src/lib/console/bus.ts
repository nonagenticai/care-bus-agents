import type { AgentView, BusEvent, Entry } from '$lib/api/types';
import { busKey } from './console.svelte';

/**
 * Everything here is derived from the envelopes the console receives. Which group an entry is for comes from the
 * server (BusEvent.consumer_group); `consumerOf` below mirrors the bus topology in
 * crates/carebus (interaction, orchestrator, worker, executor). Who consumes a topic is fixed by that code:
 * request.qualified → orchestrator, request.resolved → interaction, work.requested on
 * bus:worker:{area}:{tool} → worker-{tool}, work.succeeded/failed → orchestrator, tool.call → tool-executor,
 * tool.response on bus:tool:{tool} → worker-{tool}.
 */
export const EVENTS = [
	'request.qualified',
	'request.resolved',
	'work.requested',
	'work.succeeded',
	'work.failed',
	'tool.call',
	'tool.response',
] as const;
export type BusEventName = (typeof EVENTS)[number];

/** `logistics.delivery-status.tool.call` → `tool.call` (crates/carebus domain.rs event_of). */
export function eventOf(topic: string): BusEventName | null {
	return EVENTS.find((e) => topic === e || (topic.endsWith(`.${e}`) && topic.length > e.length + 1)) ?? null;
}
/** The context-space part of a topic, with its trailing dot: `logistics.delivery-status.`. */
export function contextOf(topic: string): string {
	const e = eventOf(topic);
	return e && topic !== e ? topic.slice(0, topic.length - e.length) : '';
}

const tail = (stream: string) => stream.split(':').at(-1) ?? '';

/**
 * Consumer group for an entry from GET /api/streams/{key}/entries, which (unlike SSE and case traces) carries no
 * `consumer_group`. Console code reads the server's `BusEvent.consumer_group` instead (carebus
 * Streams::consumer_group); this is only for the admin buses page.
 */
export function consumerOf(e: Entry): string | null {
	switch (eventOf(e.envelope.topic)) {
		case 'request.qualified':
		case 'work.succeeded':
		case 'work.failed':
			return 'orchestrator';
		case 'request.resolved':
			return 'interaction';
		case 'work.requested':
		case 'tool.response':
			return `worker-${tail(e.stream)}`;
		case 'tool.call':
			return 'tool-executor';
		default:
			return null;
	}
}
/** Agent id of the publisher: `worker:order` → `worker-order`, `mcp:order` → `tool-executor`. */
export function producerOf(from: string): string {
	if (from.startsWith('worker:')) return `worker-${from.slice(7)}`;
	if (from.startsWith('mcp:')) return 'tool-executor';
	return from;
}

const obj = (v: unknown): Record<string, unknown> =>
	v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : {};
const str = (v: unknown) => (typeof v === 'string' || typeof v === 'number' ? String(v) : '');

/** One-line summary of the payload, built from the fields carebus actually publishes (domain.rs). */
export function summary(e: Entry): string {
	const p = obj(e.envelope.payload);
	const work = obj(p.work);
	switch (eventOf(e.envelope.topic)) {
		case 'request.qualified': {
			const slots = obj(p.slots);
			const key = str(slots.order_id) || str(slots.question);
			return [str(p.request_type), key].filter(Boolean).join(' · ');
		}
		case 'request.resolved':
			return [str(p.outcome), str(p.reason)].filter(Boolean).join(' · ');
		case 'work.requested': {
			const attempt = Number(p.attempt) || 1;
			return `${str(p.worker)}.${str(p.action)}${attempt > 1 ? ` (retry ${attempt - 1})` : ''}`;
		}
		case 'work.succeeded':
			return `${str(p.worker)}.${str(p.action)} → ok`;
		case 'work.failed':
			return `${str(p.worker)}.${str(p.action)} → ${str(p.error) || 'failed'}`;
		case 'tool.call': {
			const args = Object.entries(obj(p.arguments))
				.map(([k, v]) => `${k}=${v !== null && typeof v === 'object' ? '[…]' : String(v)}`)
				.join(', ');
			return `${str(p.mcp_tool) || str(work.action)}(${args})`;
		}
		case 'tool.response':
			if (p.ok === false) return `error · ${str(p.error) || 'unknown'}`;
			return [p.ok === true ? 'ok' : '', p.latency_ms !== undefined ? `${str(p.latency_ms)} ms` : '']
				.filter(Boolean)
				.join(' · ');
		default:
			return '';
	}
}

const words = (v: unknown) => str(v).replaceAll('_', ' ').replaceAll('-', ' ');
const NAMES: Record<string, string> = {
	crm: 'customer records',
	order: 'order',
	delivery: 'delivery',
	faq: 'FAQ',
	'business-rules': 'business rules',
	procedures: 'procedures',
};
const worker = (v: unknown) => `${NAMES[str(v)] ?? words(v)} worker`;

/**
 * Plain-language sentence for an event, from the fields carebus publishes (domain.rs): QualifiedRequest
 * (request_type, slots), WorkRequest/WorkOutcome (worker, action, attempt, error), ToolCall/ToolResponse
 * (mcp_tool, ok, error, latency_ms) and Resolution (outcome, reason). Technical ids stay in the row below it.
 */
export function plain(e: Entry): string {
	const p = obj(e.envelope.payload);
	const tool = str(p.mcp_tool) || str(obj(p.work).action);
	switch (eventOf(e.envelope.topic)) {
		case 'request.qualified': {
			const slots = obj(p.slots);
			const about = str(slots.order_id) || str(slots.question);
			return `Customer request understood as ${words(p.request_type) || 'a request'}${about ? ` (${about})` : ''}; handed to the orchestrator.`;
		}
		case 'request.resolved': {
			const why = str(p.reason);
			const outcome = str(p.outcome);
			const what =
				outcome === 'completed'
					? 'completed'
					: outcome === 'not_eligible'
						? 'closed as not eligible'
						: outcome === 'failed'
							? 'failed'
							: words(outcome) || 'resolved';
			return `Case ${what}${why ? `: ${why}` : ''}.`;
		}
		case 'work.requested': {
			const attempt = Number(p.attempt) || 1;
			return `Orchestrator asked the ${worker(p.worker)} to ${words(p.action)}${attempt > 1 ? ` (retry ${attempt - 1})` : ''}.`;
		}
		case 'work.succeeded':
			return `The ${worker(p.worker)} finished: ${words(p.action)}.`;
		case 'work.failed':
			return `The ${worker(p.worker)} could not ${words(p.action)}${str(p.error) ? `: ${str(p.error)}` : ''}.`;
		case 'tool.call':
			return `Calling enterprise tool ${tool || '(unnamed)'}.`;
		case 'tool.response':
			if (p.ok === false)
				return `Tool ${str(p.mcp_tool) || 'call'} failed: ${str(p.error) || 'unknown error'}.`;
			return `Tool ${str(p.mcp_tool) || 'call'} answered${p.latency_ms !== undefined ? ` in ${str(p.latency_ms)} ms` : ''}.`;
		default:
			return '';
	}
}

export interface EventLink {
	kind: 'case' | 'tool' | 'procedure';
	label: string;
	href: string;
}
/** Where an event's own data names a case, MCP tool or procedure version. Nothing is guessed from other ids. */
export function links(e: Entry): EventLink[] {
	const p = obj(e.envelope.payload);
	const out: EventLink[] = [];
	const caseId = str(p.case_id) || str(obj(p.work).case_id);
	if (caseId)
		out.push({ kind: 'case', label: caseId, href: `/admin/cases?case=${encodeURIComponent(caseId)}` });
	const tool = str(p.mcp_tool);
	if (tool) out.push({ kind: 'tool', label: tool, href: '/admin/tools' });
	const proc = e.envelope.procedure;
	if (proc) out.push({ kind: 'procedure', label: proc, href: '/admin/procedures' });
	return out;
}

/** The case an event belongs to (payload `case_id`, also inside a tool call's `work`), or ''. */
export function caseOf(e: Entry): string {
	const p = obj(e.envelope.payload);
	return str(p.case_id) || str(obj(p.work).case_id);
}

/** A failure on the bus: work.failed, a failed tool response, a failed resolution, or a dead letter. */
export function isBad(e: Entry): boolean {
	const p = obj(e.envelope.payload);
	const ev = eventOf(e.envelope.topic);
	return (
		ev === 'work.failed' ||
		(ev === 'tool.response' && p.ok === false) ||
		(ev === 'request.resolved' && p.outcome === 'failed') ||
		e.stream === 'bus:dead'
	);
}

/** Forward (down) events and the return event that answers each, matched on the id carebus carries. */
const REPLY: Partial<Record<BusEventName, BusEventName[]>> = {
	'request.qualified': ['request.resolved'],
	'work.requested': ['work.succeeded', 'work.failed'],
	'tool.call': ['tool.response'],
};
function matchId(e: BusEvent): string {
	const p = obj(e.envelope.payload);
	switch (eventOf(e.envelope.topic)) {
		case 'request.qualified':
		case 'request.resolved':
			return str(p.case_id) || e.envelope.correlation_id;
		case 'work.requested':
		case 'work.succeeded':
		case 'work.failed':
			return str(p.work_id);
		case 'tool.call':
		case 'tool.response':
			return str(p.call_id);
		default:
			return '';
	}
}

export type ItemStatus = 'run' | 'ok' | 'fail';
export interface WorkItem {
	key: string;
	event: BusEvent;
	reply: BusEvent | null;
	status: ItemStatus;
}

/** Every forward event with its answer (if one arrived). `run` means nothing has answered it yet. */
export function workItems(events: BusEvent[]): WorkItem[] {
	const replies = new Map<string, BusEvent>();
	for (const e of events) {
		const ev = eventOf(e.envelope.topic);
		const id = matchId(e);
		if (!ev || !id) continue;
		for (const [down, ups] of Object.entries(REPLY)) if (ups!.includes(ev)) replies.set(`${down}|${id}`, e);
	}
	const out: WorkItem[] = [];
	for (const e of events) {
		const ev = eventOf(e.envelope.topic);
		if (!ev || !REPLY[ev]) continue;
		const reply = replies.get(`${ev}|${matchId(e)}`) ?? null;
		out.push({ key: busKey(e), event: e, reply, status: !reply ? 'run' : isBad(reply) ? 'fail' : 'ok' });
	}
	return out;
}

export type AgentStatus = 'working' | 'waiting' | 'holding' | 'ready' | 'backlog' | 'offline' | 'unknown';
export interface AgentCard {
	id: string;
	name: string;
	kind: AgentView['kind'];
	view: AgentView | null;
	status: AgentStatus;
	line: string;
	/** Forward events this agent consumes or published, newest last. */
	items: WorkItem[];
}

/** The agents carebus-api lists (routes/agents.rs catalogue), used until /api/agents answers. */
export const CATALOGUE: Pick<AgentView, 'id' | 'name' | 'kind'>[] = [
	{ id: 'interaction', name: 'Interaction agent', kind: 'front' },
	{ id: 'orchestrator', name: 'Orchestrator agent', kind: 'coordination' },
	...['crm', 'order', 'delivery', 'faq', 'business-rules', 'procedures'].map((t) => ({
		id: `worker-${t}`,
		name: `${t} worker`,
		kind: 'worker' as const,
	})),
	{ id: 'tool-executor', name: 'Tool executor (MCP bridge)', kind: 'bridge' },
];

/**
 * Agent cards for the strip. Status, most specific first: `working` (it consumes a forward event nobody has
 * answered yet), `waiting` (it published one that is still unanswered), `holding` (the interaction agent keeps
 * a request back because the last turn asked for more detail), then from /api/agents: `backlog` (status warn),
 * `ready` (has consumers), `offline` (no consumers), `unknown` (the API did not answer).
 */
export function agentCards(events: BusEvent[], views: AgentView[] | null, holding: boolean): AgentCard[] {
	const items = workItems(events);
	const list =
		views && views.length
			? views.map((v) => ({ id: v.id, name: v.name, kind: v.kind, view: v as AgentView | null }))
			: CATALOGUE.map((c) => ({ ...c, view: null as AgentView | null }));
	return list.map(({ id, name, kind, view }) => {
		const a = { id };
		const mine = items.filter(
			(w) => w.event.consumer_group === a.id || producerOf(w.event.envelope.from) === a.id,
		);
		const consuming = mine.filter((w) => w.status === 'run' && w.event.consumer_group === a.id);
		const publishing = mine.filter((w) => w.status === 'run' && producerOf(w.event.envelope.from) === a.id);
		const last = [...events]
			.reverse()
			.find((e) => e.consumer_group === a.id || producerOf(e.envelope.from) === a.id);
		let status: AgentStatus;
		if (consuming.length) status = 'working';
		else if (publishing.length) status = 'waiting';
		else if (a.id === 'interaction' && holding) status = 'holding';
		else if (!view) status = 'unknown';
		else if (view.status === 'warn') status = 'backlog';
		else if (view.consumers > 0) status = 'ready';
		else status = 'offline';
		let line = '—';
		if (last) {
			const out = producerOf(last.envelope.from) === a.id;
			const s = summary(last);
			line = `${out ? '→' : '←'} ${eventOf(last.envelope.topic) ?? last.envelope.topic}${s ? ` · ${s}` : ''}`;
		} else if (a.id === 'interaction' && holding) line = 'asked the customer for more detail';
		return { id, name, kind, view, status, line, items: mine };
	});
}

/** JSON tokens for the inspector's coloured <pre>: k key, s string, n number, b boolean/null, p punctuation. */
export type Tok = { t: string; c: '' | 'k' | 's' | 'n' | 'b' };
export function tokens(value: unknown): Tok[] {
	const src = JSON.stringify(value, null, 2) ?? 'undefined';
	const re = /("(?:[^"\\]|\\.)*")(\s*:)?|\b(true|false|null)\b|-?\b\d+(?:\.\d+)?(?:[eE][+-]?\d+)?\b/g;
	const out: Tok[] = [];
	let i = 0;
	for (let m = re.exec(src); m; m = re.exec(src)) {
		if (m.index > i) out.push({ t: src.slice(i, m.index), c: '' });
		if (m[1]) {
			out.push({ t: m[1], c: m[2] ? 'k' : 's' });
			if (m[2]) out.push({ t: m[2], c: '' });
		} else out.push({ t: m[0], c: m[3] ? 'b' : 'n' });
		i = m.index + m[0].length;
	}
	if (i < src.length) out.push({ t: src.slice(i), c: '' });
	return out;
}

/** `HH:MM:SS.mmm` in local time. */
export function clock(ms: number): string {
	const d = new Date(ms);
	if (Number.isNaN(d.getTime())) return '';
	const p = (n: number, w = 2) => String(n).padStart(w, '0');
	return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}.${p(d.getMilliseconds(), 3)}`;
}

/**
 * What the bus shows about one case: the case in view (the trace filter, else the newest event's correlation id),
 * the latest observed step, the outcome once request.resolved was seen, and when its first and terminal events
 * were published. Built only from received envelopes; nothing about agent state is inferred.
 */
export interface Activity {
	caseId: string;
	events: number;
	step: string;
	stepAt: number | null;
	outcome: string | null;
	bad: boolean;
	startedAt: number | null;
	endedAt: number | null;
}
export function activity(events: BusEvent[], caseFilter = ''): Activity | null {
	const caseId = caseFilter || events.at(-1)?.envelope.correlation_id || '';
	if (!caseId) return null;
	const mine = events.filter((e) => e.envelope.correlation_id === caseId);
	const last = mine.at(-1);
	const resolved = mine.findLast((e) => eventOf(e.envelope.topic) === 'request.resolved') ?? null;
	const times = mine.map((e) => e.envelope.published_at_ms).filter((t) => Number.isFinite(t));
	return {
		caseId,
		events: mine.length,
		step: last ? plain(last) || summary(last) || last.envelope.topic : '',
		stepAt: last?.envelope.published_at_ms ?? null,
		outcome: resolved ? plain(resolved) || summary(resolved) || 'resolved' : null,
		bad: resolved ? isBad(resolved) : false,
		startedAt: times.length ? Math.min(...times) : null,
		endedAt: resolved?.envelope.published_at_ms ?? null,
	};
}

/** Compact duration: `42s`, `3m 05s`, `2h 07m`, `1d 04h`. Negative spans read as 0s. */
export function elapsed(ms: number): string {
	const s = Math.max(0, Math.floor(ms / 1000));
	const p = (n: number) => String(n).padStart(2, '0');
	if (s < 60) return `${s}s`;
	const m = Math.floor(s / 60);
	if (m < 60) return `${m}m ${p(s % 60)}s`;
	const h = Math.floor(m / 60);
	if (h < 24) return `${h}h ${p(m % 60)}m`;
	return `${Math.floor(h / 24)}d ${p(h % 24)}h`;
}
