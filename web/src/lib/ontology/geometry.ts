// Pure edge geometry for the ontology graph. The edge component draws the same cubic from the measured handle
// points that layout.ts uses (with estimated points) to place the labels, so a label chosen here lands on the line
// that is drawn. Node boxes are given an explicit height (nodeHeight) so the estimate matches what renders.

export type Side = 'left' | 'right';
export interface Pt {
	x: number;
	y: number;
}
export interface Rect {
	x: number;
	y: number;
	w: number;
	h: number;
}
export type Cubic = [Pt, Pt, Pt, Pt];

export const NODE_W = 212;
/** Attribute rows shown before the card scrolls. */
export const MAX_ROWS = 10;
/** Border 2 + head 37 + list padding 16 + rows of 17px with a 2px gap (see OntologyEntityNode). */
export const nodeHeight = (attributes: number) => 53 + 19 * Math.min(Math.max(attributes, 1), MAX_ROWS);
/** How far a handle's attachment point sits outside the card border. */
export const HANDLE_OUT = 4;

const dir = (s: Side) => (s === 'left' ? -1 : 1);

/**
 * A cubic leaving `s` and entering `t` horizontally on the given sides. Opposite sides give the usual S-curve;
 * the same side (two cards in one column) bows outward so the line never cuts through the cards.
 */
export function curve(s: Pt, sSide: Side, t: Pt, tSide: Side): Cubic {
	const k = sSide === tSide ? 40 + 0.2 * Math.abs(t.y - s.y) : Math.max(40, 0.5 * Math.abs(t.x - s.x));
	return [s, { x: s.x + dir(sSide) * k, y: s.y }, { x: t.x + dir(tSide) * k, y: t.y }, t];
}

export const pathD = ([a, b, c, d]: Cubic) => `M${a.x},${a.y} C${b.x},${b.y} ${c.x},${c.y} ${d.x},${d.y}`;

export function pointAt([a, b, c, d]: Cubic, t: number): Pt {
	const u = 1 - t;
	const w0 = u * u * u,
		w1 = 3 * u * u * t,
		w2 = 3 * u * t * t,
		w3 = t * t * t;
	return {
		x: w0 * a.x + w1 * b.x + w2 * c.x + w3 * d.x,
		y: w0 * a.y + w1 * b.y + w2 * c.y + w3 * d.y,
	};
}

/** An edge path: one cubic, or three when a link skips a column (in, straight through a gap, out). */
export type Route = Cubic[];
/** The horizontal passage a column-skipping link takes through the gap between two cards of the skipped column. */
export interface Via {
	y: number;
	x1: number;
	x2: number;
}
const flip = (s: Side): Side => (s === 'left' ? 'right' : 'left');
const line = (a: Pt, b: Pt): Cubic => [a, a, b, b];

/**
 * A link that skips a column runs straight through `via`, a gap between that column's cards, so it never passes
 * behind a card it has nothing to do with. Every join keeps a horizontal tangent, so the route stays smooth.
 */
export function route(s: Pt, sSide: Side, t: Pt, tSide: Side, via: Via | null = null): Route {
	if (!via) return [curve(s, sSide, t, tSide)];
	const [a, b] = sSide === 'right' ? [via.x1, via.x2] : [via.x2, via.x1];
	const [p, q] = [
		{ x: a, y: via.y },
		{ x: b, y: via.y },
	];
	return [curve(s, sSide, p, flip(sSide)), line(p, q), curve(q, sSide, t, tSide)];
}

export const routeD = (r: Route) =>
	r.map((c, i) => (i ? pathD(c).replace(/^M[^C]*/, '') : pathD(c))).join(' ');

/** Point at fraction `t` of the route, each segment taking an equal share. */
export function routeAt(r: Route, t: number): Pt {
	const i = Math.min(Math.floor(t * r.length), r.length - 1);
	return pointAt(r[i]!, t * r.length - i);
}

/** Approximate rendered size of "predicate card" in the 10.5px / 9px mono label (generous, never under). */
export function labelSize(predicate: string, cardinality: string): { w: number; h: number } {
	return { w: Math.ceil(predicate.length * 6.6 + cardinality.length * 5.7 + 4 + 12), h: 18 };
}

export const overlaps = (a: Rect, b: Rect, pad = 0) =>
	a.x < b.x + b.w + pad && b.x < a.x + a.w + pad && a.y < b.y + b.h + pad && b.y < a.y + a.h + pad;

const box = (p: Pt, w: number, h: number): Rect => ({ x: p.x - w / 2, y: p.y - h / 2, w, h });

/** Where along the curve a label may sit, best first. */
export const LABEL_TS = [0.5, 0.42, 0.58, 0.34, 0.66, 0.26, 0.74, 0.2, 0.8];
const NUDGES = [1, -1, 2, -2, 3, -3, 4, -4];
const LABEL_PAD = 3;
const NODE_PAD = 4;

export interface LabelPlace {
	t: number;
	dx: number;
	dy: number;
}

/**
 * Greedy, deterministic label placement: each label takes the first position along its own curve that touches
 * neither a card nor a label already placed; failing that it is nudged vertically off the midpoint until it is free.
 */
export function placeLabels(
	items: { route: Route; w: number; h: number }[],
	obstacles: Rect[],
): LabelPlace[] {
	const placed: Rect[] = [];
	const free = (r: Rect) =>
		!placed.some((p) => overlaps(r, p, LABEL_PAD)) && !obstacles.some((o) => overlaps(r, o, NODE_PAD));
	return items.map(({ route: c, w, h }) => {
		let pick: LabelPlace | null = null;
		for (const t of LABEL_TS) {
			if (free(box(routeAt(c, t), w, h))) {
				pick = { t, dx: 0, dy: 0 };
				break;
			}
		}
		outer: for (const t of pick ? [] : LABEL_TS.slice(0, 3)) {
			for (const n of NUDGES) {
				const dy = n * (h + LABEL_PAD);
				const p = routeAt(c, t);
				if (free(box({ x: p.x, y: p.y + dy }, w, h))) {
					pick = { t, dx: 0, dy };
					break outer;
				}
			}
		}
		pick ??= { t: 0.5, dx: 0, dy: 0 };
		const p = routeAt(c, pick.t);
		placed.push(box({ x: p.x + pick.dx, y: p.y + pick.dy }, w, h));
		return pick;
	});
}

/** Proper crossing of two segments (shared endpoints and collinear touches do not count). */
export function segmentsCross(a: Pt, b: Pt, c: Pt, d: Pt): boolean {
	const o = (p: Pt, q: Pt, r: Pt) => Math.sign((q.x - p.x) * (r.y - p.y) - (q.y - p.y) * (r.x - p.x));
	return o(a, b, c) * o(a, b, d) < 0 && o(c, d, a) * o(c, d, b) < 0;
}
