import type {
	BusEvent,
	CaseRow,
	ContextSpace,
	DeadLetterPayload,
	Entry,
	GroupInfo,
	Outcome,
	StreamInfo,
} from '$lib/api/types';
import { laneOf, type Lane } from '$lib/console/console.svelte';
import { CATALOGUE, producerOf } from '$lib/console/bus';

export function deadLetter(e: Entry): DeadLetterPayload | null {
	const p = e.envelope.payload as Partial<DeadLetterPayload> | null;
	return p && typeof p.stream === 'string' && typeof p.reason === 'string' && p.original
		? (p as DeadLetterPayload)
		: null;
}

export function groupHealth(g: GroupInfo): 'ok' | 'warn' | 'err' {
	const lag = g.lag ?? 0;
	if (lag > 20 || g.pending > 10) return 'err';
	if (lag > 5 || g.pending > 3) return 'warn';
	return 'ok';
}

export function streamHealth(s: StreamInfo): { word: 'ok' | 'warn' | 'err'; label: string } {
	if (s.bus === 'dead')
		return s.length > 0 ? { word: 'err', label: 'Has entries' } : { word: 'ok', label: 'Healthy' };
	const hs = s.groups.map(groupHealth);
	if (hs.includes('err')) return { word: 'err', label: 'Behind' };
	if (hs.includes('warn')) return { word: 'warn', label: 'Falling behind' };
	return { word: 'ok', label: 'Healthy' };
}

export function traceRows(trace: BusEvent[]): { lane: Lane; t_ms: number; ev: BusEvent }[] {
	const ms = (e: BusEvent) => Number(e.entry_id.split('-')[0]);
	const seq = (e: BusEvent) => Number(e.entry_id.split('-')[1]);
	const sorted = trace.slice().sort((a, b) => ms(a) - ms(b) || seq(a) - seq(b));
	const t0 = sorted.length ? ms(sorted[0]!) : 0;
	return sorted.map((ev) => ({ lane: laneOf(ev.stream), t_ms: ms(ev) - t0, ev }));
}

export { areaLabel, areasOf } from '$lib/areas';
export const outcomeLabel = (o: Outcome | null) =>
	o === null ? 'Open' : o === 'not_eligible' ? 'Not eligible' : o === 'completed' ? 'Completed' : 'Failed';

/**
 * Context-space labels from GET /api/context-spaces; raw keys when the list has not loaded.
 * A space's label doubles as its request type's label (one request type per space), so a case row shows the
 * label once and uses `procedure` -- the procedure name the space runs -- as its second line.
 */
export function spaceLabels(spaces: ContextSpace[]) {
	const byKey = new Map(spaces.map((s) => [s.key, s.label]));
	const byType = new Map(spaces.map((s) => [s.request_type, s.label]));
	const procs = new Map(spaces.map((s) => [s.key, s.procedure]));
	return {
		space: (key: string) => byKey.get(key) ?? key,
		type: (t: string) => byType.get(t) ?? t.replaceAll('_', ' '),
		procedure: (key: string): string | null => procs.get(key) ?? null,
	};
}

/**
 * What a case shows as its procedure. `recorded` is the `name@version` the case actually ran (the orchestrator stamps
 * it on request.resolved). Open cases and cases recorded before that stamp existed have none; they fall back to the
 * procedure name the context space runs (`derived`) -- a name only, not a version, and not proof this case ran it.
 */
export function caseProcedure(
	c: Pick<CaseRow, 'procedure' | 'context_space'>,
	derive: (key: string) => string | null,
): { text: string; recorded: boolean } | null {
	if (c.procedure) return { text: c.procedure, recorded: true };
	const name = derive(c.context_space);
	return name ? { text: name, recorded: false } : null;
}

/** Publisher name for an envelope's `from`: `worker:order` → "order worker", per the agents catalogue. */
export function agentName(from: string): string {
	const id = producerOf(from);
	return CATALOGUE.find((a) => a.id === id)?.name ?? from;
}
