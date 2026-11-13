import type { Entry, GroupInfo } from '$lib/api/types';
import { CATALOGUE, clock, consumerOf, eventOf, producerOf, summary } from '$lib/console/bus';
import { areaLabel } from '$lib/areas';

export { areaLabel };

/**
 * Human name for a stream key, from the key shapes in crates/carebus/src/bus.rs: bus:customer:{area},
 * bus:worker:{area}:{tool}, bus:worker:{area}:results, bus:tool:{tool}, bus:dead. Anything else keeps its key.
 */
export function streamLabel(key: string): string {
	const p = key.split(':');
	if (p[0] !== 'bus') return key;
	if (p[1] === 'customer' && p.length === 3) return `${areaLabel(p[2]!)} · customer bus`;
	if (p[1] === 'worker' && p.length === 4)
		return p[3] === 'results' ? `${areaLabel(p[2]!)} · work results` : `${areaLabel(p[2]!)} · ${p[3]} work`;
	if (p[1] === 'tool' && p.length === 3) return `${p[2]} tool calls`;
	if (p[1] === 'dead' && p.length === 2) return 'Dead letters';
	return key;
}

/** carebus-api's own group (crates/carebus-api/src/recorder.rs) writes case_history and tool audit rows. */
export const RECORDER_GROUP = 'carebus-api-recorder';

/** Agent or consumer-group id → its name (routes/agents.rs catalogue); unknown ids stay as they are. */
export function agentName(id: string): string {
	if (id === RECORDER_GROUP) return 'Case history recorder';
	return CATALOGUE.find((c) => c.id === id)?.name ?? id;
}
/** Envelope.from → publisher name: `worker:order` is the order worker, `mcp:order` the tool executor. */
export const publisherName = (from: string) => agentName(producerOf(from));

export const groupChipText = (g: GroupInfo) =>
	`${agentName(g.name)} · ${g.pending} pending · lag ${g.lag ?? '—'}`;

/** One-line view of a bus entry: event, publisher → consumer group, publish time and the payload summary. */
export function entryLine(e: Entry): {
	event: string;
	from: string;
	to: string | null;
	time: string;
	detail: string;
} {
	const to = consumerOf(e);
	return {
		event: eventOf(e.envelope.topic) ?? e.envelope.topic,
		from: publisherName(e.envelope.from),
		to: to ? agentName(to) : null,
		time: clock(e.envelope.published_at_ms),
		detail: summary(e),
	};
}

/** Time part of a stream entry id (`<ms>-<seq>`), or '' when the id carries none. */
export function entryTime(id: string): string {
	const ms = Number(id.split('-')[0]);
	return Number.isFinite(ms) && ms > 0 ? clock(ms) : '';
}

export const ROLE_HINT = 'Needs the tenant_admin role';
/** Why Replay is disabled, or '' when it is allowed. */
export function replayBlocked(admin: boolean, isDeadLetter: boolean): string {
	if (!admin) return ROLE_HINT;
	if (!isDeadLetter) return 'Not a dead letter: it names no stream to replay to';
	return '';
}
/** Why Claim stuck is disabled, or '' when it is allowed. carebus-api claims entries idle 30 s or more. */
export function claimBlocked(admin: boolean, pending: number): string {
	if (!admin) return ROLE_HINT;
	if (pending === 0) return 'Nothing is pending in this group';
	return '';
}
