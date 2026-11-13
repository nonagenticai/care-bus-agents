// Pure ontology → graph layout. Entities have no stored coordinates, so they are placed in one column per
// scope (generic | industry | industry_dept), ordered within the column to minimise edge crossings. Relations become edges
// from subject to object; a relation whose end is not on the canvas (filtered out, hidden, or unknown) is dropped
// rather than drawn to nowhere.
import type { Edge, Node } from '@xyflow/svelte';
import type { Cardinality, ContextSpace, OntologyEntity, OntologyRelation, Scope } from '$lib/api/types';
import {
	HANDLE_OUT,
	NODE_W,
	labelSize,
	nodeHeight,
	placeLabels,
	route,
	segmentsCross,
	type LabelPlace,
	type Pt,
	type Side,
	type Via,
} from './geometry';

export const COLUMN_X: Record<Scope, number> = { generic: 40, industry: 400, industry_dept: 760 };
export const Y_TOP = 24;
export const Y_STEP = 190;

export const SCOPE_LABEL: Record<Scope, string> = {
	generic: 'Generic',
	industry: 'Industry',
	industry_dept: 'Specialized',
};

export type EntityNodeData = { entity: OntologyEntity };
/** `label`: where the label sits on the route; `via`: the waypoint of a link that skips a column. */
export type RelationEdgeData = { relation: OntologyRelation; label: LabelPlace; via: Via | null };
export type EntityNode = Node<EntityNodeData, 'entity'>;
export type RelationEdge = Edge<RelationEdgeData, 'relation'>;

export interface Filter {
	/** 'all' or an industry value (e.g. 'retail'). */
	industry: string;
	/** 'all' or a department value; only narrows industry_dept entities. */
	department: string;
	/** 'all' or a context-space key; narrows to the entities that space lists. */
	space: string;
	showHidden: boolean;
}
export const ALL: Filter = { industry: 'all', department: 'all', space: 'all', showHidden: false };

/** Distinct non-null values, sorted, for the industry / department chips. */
export function distinct(
	entities: OntologyEntity[],
	key: 'industry' | 'department',
	industry = 'all',
): string[] {
	const out = new Set<string>();
	for (const e of entities) {
		if (key === 'department' && industry !== 'all' && e.industry !== industry) continue;
		const v = e[key];
		if (v) out.add(v);
	}
	return [...out].sort();
}

export function visibleEntities(
	entities: OntologyEntity[],
	f: Filter,
	spaces: ContextSpace[] = [],
): OntologyEntity[] {
	const space = f.space === 'all' ? null : spaces.find((s) => s.key === f.space);
	const inSpace = space ? new Set(space.entities) : null;
	return entities.filter((e) => {
		if (e.hidden && !f.showHidden) return false;
		if (inSpace && !inSpace.has(e.name)) return false;
		if (e.scope === 'generic') return true;
		if (f.industry !== 'all' && e.industry !== f.industry) return false;
		if (e.scope === 'industry_dept' && f.department !== 'all' && e.department !== f.department) return false;
		return true;
	});
}

const COLUMNS: Scope[] = ['generic', 'industry', 'industry_dept'];
/** Minimum vertical gap between two cards in a column. */
export const Y_GAP = 48;
/** How far outside the skipped column a column-skipping link straightens out. */
const VIA_MARGIN = 14;
const center = (col: number) => COLUMN_X[COLUMNS[col]!] + NODE_W / 2;

/** Top y of each card when a column is stacked in this order: Y_STEP apart, more when a card is taller. */
export function stack(column: string[], heightOf: (name: string) => number): Map<string, number> {
	const out = new Map<string, number>();
	let y = Y_TOP;
	for (const name of column) {
		out.set(name, y);
		y += Math.max(Y_STEP, heightOf(name) + Y_GAP);
	}
	return out;
}

/** Crossings between the straight centre-to-centre segments of the links that join two different columns. */
export function countCrossings(
	columns: string[][],
	links: [string, string][],
	heightOf: (name: string) => number = () => 0,
): number {
	const at = new Map<string, Pt>();
	columns.forEach((column, c) =>
		stack(column, heightOf).forEach((y, name) => at.set(name, { x: center(c), y: y + heightOf(name) / 2 })),
	);
	const segs = links
		.filter(([a, b]) => at.has(a) && at.has(b) && at.get(a)!.x !== at.get(b)!.x)
		.map(([a, b]) => ({ a, b, p: at.get(a)!, q: at.get(b)! }));
	let n = 0;
	for (let i = 0; i < segs.length; i++)
		for (let j = i + 1; j < segs.length; j++) {
			const s = segs[i]!,
				t = segs[j]!;
			if (s.a === t.a || s.a === t.b || s.b === t.a || s.b === t.b) continue;
			if (segmentsCross(s.p, s.q, t.p, t.q)) n++;
		}
	return n;
}

/**
 * Barycentric ordering: each card moves to the mean height of the cards it links to in other columns, sweeping
 * left-to-right then right-to-left. Ties keep the current order, so the result is deterministic, and the best order
 * seen (starting with the API order) wins, so it never has more crossings than the input.
 */
export function orderColumns(
	columns: string[][],
	links: [string, string][],
	heightOf: (name: string) => number = () => 0,
	sweeps = 8,
): string[][] {
	const colOf = new Map<string, number>();
	columns.forEach((column, c) => column.forEach((n) => colOf.set(n, c)));
	const nbrs = new Map<string, string[]>();
	for (const [a, b] of links) {
		if (!colOf.has(a) || !colOf.has(b) || colOf.get(a) === colOf.get(b)) continue;
		nbrs.set(a, [...(nbrs.get(a) ?? []), b]);
		nbrs.set(b, [...(nbrs.get(b) ?? []), a]);
	}
	let cur = columns.map((c) => [...c]);
	let best = cur;
	let bestN = countCrossings(cur, links, heightOf);
	const mid = (ys: Map<string, number>, n: string) => ys.get(n)! + heightOf(n) / 2;
	for (let s = 0; s < sweeps && bestN > 0; s++) {
		const seq = cur.map((_, i) => i);
		if (s % 2) seq.reverse();
		for (const c of seq) {
			const ys = new Map<string, number>();
			cur.forEach((column) => stack(column, heightOf).forEach((y, n) => ys.set(n, y)));
			const bary = (n: string) => {
				const ns = nbrs.get(n) ?? [];
				return ns.length ? ns.reduce((sum, m) => sum + mid(ys, m), 0) / ns.length : mid(ys, n);
			};
			const keyed = cur[c]!.map((n, i) => ({ n, i, b: bary(n) }));
			keyed.sort((x, y) => x.b - y.b || x.i - y.i);
			cur = cur.map((column, i) => (i === c ? keyed.map((k) => k.n) : column));
		}
		const n = countCrossings(cur, links, heightOf);
		if (n < bestN) {
			best = cur;
			bestN = n;
		}
	}
	return best;
}

export function layout(
	entities: OntologyEntity[],
	relations: OntologyRelation[],
): { nodes: EntityNode[]; edges: RelationEdge[] } {
	// An unknown scope from a newer API would otherwise land at x=undefined; park it in the last column.
	const scopeOf = (e: OntologyEntity): Scope => (e.scope in COLUMN_X ? e.scope : 'industry_dept');
	const byName = new Map(entities.map((e) => [e.name, e]));
	const colOf = new Map(entities.map((e) => [e.name, COLUMNS.indexOf(scopeOf(e))]));
	const heightOf = (n: string) => nodeHeight(byName.get(n)?.attributes.length ?? 0);
	const rels = relations.filter((r) => byName.has(r.subject) && byName.has(r.object));
	const links = rels.map((r): [string, string] => [r.subject, r.object]);
	const columns = orderColumns(
		COLUMNS.map((_, c) => entities.filter((e) => colOf.get(e.name) === c).map((e) => e.name)),
		links,
		heightOf,
	);

	const pos = new Map<string, Pt>();
	const nodes: EntityNode[] = [];
	columns.forEach((column, c) =>
		stack(column, heightOf).forEach((y, name) => {
			const position = { x: COLUMN_X[COLUMNS[c]!], y };
			pos.set(name, position);
			nodes.push({
				id: name,
				type: 'entity',
				position,
				width: NODE_W,
				height: heightOf(name),
				data: { entity: byName.get(name)! },
			});
		}),
	);

	// Two cards in one column are joined on the side of that column facing fewer cross-column links,
	// which for the outer columns is the outside of the canvas.
	const loopSide = COLUMNS.map((_, c): Side => {
		let left = 0,
			right = 0;
		for (const [a, b] of links) {
			const [ca, cb] = [colOf.get(a)!, colOf.get(b)!];
			const other = ca === c ? cb : cb === c ? ca : c;
			if (other < c) left++;
			else if (other > c) right++;
		}
		return left <= right ? 'left' : 'right';
	});
	const sides = (r: OntologyRelation): [Side, Side] => {
		const [cs, ct] = [colOf.get(r.subject)!, colOf.get(r.object)!];
		if (cs < ct) return ['right', 'left'];
		if (cs > ct) return ['left', 'right'];
		return [loopSide[cs]!, loopSide[cs]!];
	};
	const handle = (name: string, side: Side): Pt => {
		const p = pos.get(name)!;
		return {
			x: side === 'left' ? p.x - HANDLE_OUT : p.x + NODE_W + HANDLE_OUT,
			y: p.y + heightOf(name) / 2,
		};
	};
	// A link that skips a column threads the gap between that column's cards nearest its straight line.
	const via = (r: OntologyRelation, s: Pt, t: Pt): Via | null => {
		const [cs, ct] = [colOf.get(r.subject)!, colOf.get(r.object)!];
		if (Math.abs(cs - ct) < 2) return null;
		const mid = (cs + ct) / 2;
		const cards = columns[mid]!.map((n) => ({ top: pos.get(n)!.y, bottom: pos.get(n)!.y + heightOf(n) }));
		if (!cards.length) return null;
		const gaps = [cards[0]!.top - Y_GAP / 2, cards.at(-1)!.bottom + Y_GAP / 2];
		for (let i = 1; i < cards.length; i++) gaps.push((cards[i - 1]!.bottom + cards[i]!.top) / 2);
		const want = (s.y + t.y) / 2;
		const y = gaps.reduce((best, g) => (Math.abs(g - want) < Math.abs(best - want) ? g : best));
		const x = COLUMN_X[COLUMNS[mid]!];
		return { y, x1: x - VIA_MARGIN, x2: x + NODE_W + VIA_MARGIN };
	};
	const routes = rels.map((r) => {
		const [ss, ts] = sides(r);
		const [s, t] = [handle(r.subject, ss), handle(r.object, ts)];
		const v = via(r, s, t);
		return { ss, ts, via: v, route: route(s, ss, t, ts, v) };
	});
	const placed = placeLabels(
		rels.map((r, i) => ({ route: routes[i]!.route, ...labelSize(r.predicate, r.cardinality) })),
		nodes.map((n) => ({ x: n.position.x, y: n.position.y, w: NODE_W, h: n.height! })),
	);
	const edges: RelationEdge[] = rels.map((r, i) => {
		const { ss, ts, via } = routes[i]!;
		return {
			id: `e${i}_${r.subject}_${r.predicate}_${r.object}`,
			type: 'relation',
			source: r.subject,
			target: r.object,
			sourceHandle: `${ss}-out`,
			targetHandle: `${ts}-in`,
			label: r.predicate,
			data: { relation: r, label: placed[i]!, via },
		};
	});
	return { nodes, edges };
}

/** Relations touching an entity, split by direction, for the detail drawer. */
export function relationsOf(name: string, relations: OntologyRelation[]) {
	return {
		out: relations.filter((r) => r.subject === name),
		in: relations.filter((r) => r.object === name),
	};
}

export const title = (v: string) =>
	v
		.split(/[-_\s]+/)
		.filter(Boolean)
		.map((w) => w[0]!.toUpperCase() + w.slice(1))
		.join(' ');

/** "Retail × Logistics" plus the most specific layer the filter selects. */
export function heading(f: Filter): { text: string; level: Scope | 'all' } {
	const ind = f.industry === 'all' ? 'All industries' : title(f.industry);
	const dep = f.department === 'all' ? 'All departments' : title(f.department);
	const level = f.department !== 'all' ? 'industry_dept' : f.industry !== 'all' ? 'industry' : 'all';
	return { text: `${ind} × ${dep}`, level };
}

/** Client-side validation mirroring routes::ontology::check, so the obvious mistakes never round-trip. */
export function entityFormErrors(i: {
	name: string;
	label: string;
	scope: Scope;
	industry: string | null;
	department: string | null;
	attributes: { name: string; data_type: string }[];
}): string[] {
	const e: string[] = [];
	if (!i.name.trim()) e.push('Name is required.');
	if (i.name.includes('/')) e.push('Name cannot contain /.');
	if (!i.label.trim()) e.push('Label is required.');
	if (i.scope !== 'generic' && !i.industry?.trim()) e.push('Industry is required for this scope.');
	if (i.scope === 'industry_dept' && !i.department?.trim())
		e.push('Department is required for a specialized entity.');
	if (i.attributes.some((a) => !a.name.trim() || !a.data_type.trim()))
		e.push('Every attribute needs a name and a type.');
	const names = i.attributes.map((a) => a.name.trim()).filter(Boolean);
	if (new Set(names).size !== names.length) e.push('Attribute names must be unique.');
	return e;
}

/** routes::ontology::CARDINALITIES — the server rejects anything else. */
export const CARDINALITIES: readonly Cardinality[] = ['1:1', '1:n', 'n:1', 'n:m'];
export const CARDINALITY_LABEL: Record<Cardinality, string> = {
	'1:1': 'one to one',
	'1:n': 'one to many',
	'n:1': 'many to one',
	'n:m': 'many to many',
};
/** Mirrors routes::ontology::valid_predicate: lowerCamel, letters and digits, up to 64. */
export const PREDICATE_RE = /^[a-z][A-Za-z0-9]{0,63}$/;

/** Client-side mirror of POST /api/ontology/relations validation; the server stays the authority. */
export function relationFormErrors(
	r: { subject: string; predicate: string; object: string; cardinality: string },
	entities: { name: string }[],
	relations: OntologyRelation[],
): string[] {
	const e: string[] = [];
	const known = new Set(entities.map((x) => x.name));
	if (!r.subject) e.push('Pick a subject.');
	else if (!known.has(r.subject)) e.push(`Subject ${r.subject} is not an entity.`);
	if (!r.object) e.push('Pick an object.');
	else if (!known.has(r.object)) e.push(`Object ${r.object} is not an entity.`);
	if (!r.predicate) e.push('Predicate is required.');
	else if (!PREDICATE_RE.test(r.predicate))
		e.push('Predicate must be lowerCamel letters and digits, e.g. places or paidBy.');
	if (!(CARDINALITIES as readonly string[]).includes(r.cardinality)) e.push('Pick a cardinality.');
	if (relations.some((x) => x.subject === r.subject && x.predicate === r.predicate && x.object === r.object))
		e.push(`${r.subject} ${r.predicate} ${r.object} already exists.`);
	return e;
}
