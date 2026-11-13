import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import type { ContextSpace, OntologyEntity, OntologyRelation, Paged } from '../../src/lib/api/types';
import {
	HANDLE_OUT,
	NODE_W,
	curve,
	labelSize,
	overlaps,
	placeLabels,
	route,
	routeAt,
	type Rect,
	type Side,
} from '../../src/lib/ontology/geometry';
import {
	ALL,
	COLUMN_X,
	Y_STEP,
	Y_TOP,
	countCrossings,
	distinct,
	entityFormErrors,
	heading,
	layout,
	orderColumns,
	relationFormErrors,
	relationsOf,
	visibleEntities,
} from '../../src/lib/ontology/layout';

const fx = <T>(name: string): T => JSON.parse(readFileSync(`src/lib/api/fixtures/${name}.json`, 'utf8'));
const ent = (
	name: string,
	scope: OntologyEntity['scope'],
	extra: Partial<OntologyEntity> = {},
): OntologyEntity => ({
	name,
	scope,
	industry: scope === 'generic' ? null : 'retail',
	department: scope === 'industry_dept' ? 'logistics' : null,
	label: name,
	synonyms: [],
	hidden: false,
	origin: 'base',
	overridden: false,
	attributes: [],
	...extra,
});
const rel = (subject: string, object: string, predicate = 'rel'): OntologyRelation => ({
	subject,
	predicate,
	object,
	cardinality: '1:n',
});

describe('ontology layout', () => {
	it('places each entity in its scope column, stacked in API order', () => {
		const { nodes } = layout(
			[ent('A', 'generic'), ent('B', 'industry'), ent('C', 'generic'), ent('D', 'industry_dept')],
			[],
		);
		const pos = Object.fromEntries(nodes.map((n) => [n.id, n.position]));
		expect(pos.A).toEqual({ x: COLUMN_X.generic, y: Y_TOP });
		expect(pos.C).toEqual({ x: COLUMN_X.generic, y: Y_TOP + Y_STEP });
		expect(pos.B).toEqual({ x: COLUMN_X.industry, y: Y_TOP });
		expect(pos.D).toEqual({ x: COLUMN_X.industry_dept, y: Y_TOP });
		expect(new Set([COLUMN_X.generic, COLUMN_X.industry, COLUMN_X.industry_dept]).size).toBe(3);
		expect(nodes.every((n) => n.type === 'entity')).toBe(true);
	});

	it('draws subject → object edges labelled with the predicate and drops edges with a missing end', () => {
		const { edges } = layout(
			[ent('A', 'generic'), ent('B', 'industry')],
			[rel('A', 'B', 'places'), rel('A', 'Gone'), rel('Gone', 'B'), rel('X', 'Y')],
		);
		expect(edges).toHaveLength(1);
		expect(edges[0]).toMatchObject({ source: 'A', target: 'B', label: 'places', type: 'relation' });
		expect(edges[0]!.data!.relation.cardinality).toBe('1:n');
	});

	it('gives every edge a unique id even when the same pair is related twice', () => {
		const { edges } = layout(
			[ent('A', 'generic'), ent('B', 'generic')],
			[rel('A', 'B', 'p'), rel('A', 'B', 'p')],
		);
		expect(new Set(edges.map((e) => e.id)).size).toBe(2);
	});

	it('excludes hidden entities (and their edges) unless the toggle is on', () => {
		const all = [ent('A', 'generic'), ent('B', 'industry', { hidden: true })];
		const rels = [rel('A', 'B')];
		const off = visibleEntities(all, ALL);
		expect(off.map((e) => e.name)).toEqual(['A']);
		expect(layout(off, rels).edges).toHaveLength(0);
		const on = visibleEntities(all, { ...ALL, showHidden: true });
		expect(on.map((e) => e.name)).toEqual(['A', 'B']);
		expect(layout(on, rels).edges).toHaveLength(1);
	});

	it('filters by industry and department but always keeps the generic layer', () => {
		const all = [
			ent('G', 'generic'),
			ent('R', 'industry'),
			ent('I', 'industry', { industry: 'insurance' }),
			ent('L', 'industry_dept', { department: 'logistics' }),
			ent('P', 'industry_dept', { department: 'payments' }),
		];
		const names = (f: Partial<typeof ALL>) => visibleEntities(all, { ...ALL, ...f }).map((e) => e.name);
		expect(names({})).toEqual(['G', 'R', 'I', 'L', 'P']);
		expect(names({ industry: 'retail' })).toEqual(['G', 'R', 'L', 'P']);
		expect(names({ industry: 'retail', department: 'payments' })).toEqual(['G', 'R', 'P']);
		expect(names({ industry: 'insurance' })).toEqual(['G', 'I']);
	});

	it('narrows to the entities a context space lists', () => {
		const spaces = fx<Paged<ContextSpace>>('context_spaces').items;
		const entities = fx<Paged<OntologyEntity>>('ontology_entities').items;
		const space = spaces.find((s) => s.entities.length > 1)!;
		const got = visibleEntities(entities, { ...ALL, space: space.key }, spaces).map((e) => e.name);
		expect(got.sort()).toEqual([...space.entities].filter((n) => entities.some((e) => e.name === n)).sort());
	});

	it('derives the chips and heading from the data', () => {
		const all = [
			ent('G', 'generic'),
			ent('L', 'industry_dept', { department: 'order-support' }),
			ent('I', 'industry_dept', { industry: 'insurance', department: 'claims' }),
		];
		expect(distinct(all, 'industry')).toEqual(['insurance', 'retail']);
		expect(distinct(all, 'department')).toEqual(['claims', 'order-support']);
		expect(distinct(all, 'department', 'retail')).toEqual(['order-support']);
		expect(heading({ ...ALL, industry: 'insurance', department: 'claims' })).toEqual({
			text: 'Insurance × Claims',
			level: 'industry_dept',
		});
		expect(heading({ ...ALL, industry: 'retail' }).level).toBe('industry');
		expect(heading(ALL)).toEqual({ text: 'All industries × All departments', level: 'all' });
	});

	it('lists relations in and out of an entity', () => {
		const rels = [rel('A', 'B'), rel('C', 'A'), rel('B', 'C')];
		const r = relationsOf('A', rels);
		expect(r.out.map((x) => x.object)).toEqual(['B']);
		expect(r.in.map((x) => x.subject)).toEqual(['C']);
	});

	it('lays out the demo fixtures as a connected graph', () => {
		const entities = fx<Paged<OntologyEntity>>('ontology_entities').items;
		const relations = fx<Paged<OntologyRelation>>('ontology_relations').items;
		const g = layout(visibleEntities(entities, ALL), relations);
		expect(g.nodes.length).toBeGreaterThanOrEqual(6);
		expect(g.edges.length).toBeGreaterThanOrEqual(5);
		expect(entities.some((e) => e.attributes.some((a) => a.pii))).toBe(true);
	});
});

describe('ontology graph readability', () => {
	const entities = visibleEntities(fx<Paged<OntologyEntity>>('ontology_entities').items, ALL);
	const relations = fx<Paged<OntologyRelation>>('ontology_relations').items;
	const links = relations.map((r): [string, string] => [r.subject, r.object]);
	const apiColumns = (['generic', 'industry', 'industry_dept'] as const).map((s) =>
		entities.filter((e) => e.scope === s).map((e) => e.name),
	);

	it('orders columns to remove a crossing a barycentric sweep can see', () => {
		const cols = [
			['A', 'B'],
			['C', 'D'],
		];
		const l: [string, string][] = [
			['A', 'D'],
			['B', 'C'],
		];
		expect(countCrossings(cols, l)).toBe(1);
		const got = orderColumns(cols, l);
		expect(countCrossings(got, l)).toBe(0);
		expect(got.flat().sort()).toEqual(['A', 'B', 'C', 'D']);
	});

	it('reduces crossings on the demo fixtures, deterministically, without losing a card', () => {
		const before = countCrossings(apiColumns, links);
		const ordered = orderColumns(apiColumns, links);
		expect(before).toBeGreaterThan(0);
		expect(countCrossings(ordered, links)).toBeLessThan(before);
		expect(orderColumns(apiColumns, links)).toEqual(ordered);
		ordered.forEach((col, i) => expect([...col].sort()).toEqual([...apiColumns[i]!].sort()));
	});

	it('keeps the API order when nothing links the cards', () => {
		expect(orderColumns(apiColumns, [])).toEqual(apiColumns);
	});

	it('attaches back-edges on the left and same-column edges on the outer side', () => {
		const { edges } = layout(
			[ent('G', 'generic'), ent('G2', 'generic'), ent('I', 'industry'), ent('D', 'industry_dept')],
			[rel('G', 'I'), rel('D', 'I'), rel('G', 'G2')],
		);
		const h = Object.fromEntries(
			edges.map((e) => [`${e.source}>${e.target}`, [e.sourceHandle, e.targetHandle]]),
		);
		expect(h['G>I']).toEqual(['right-out', 'left-in']);
		expect(h['D>I']).toEqual(['left-out', 'right-in']);
		expect(h['G>G2']).toEqual(['left-out', 'left-in']);
	});

	it('places no fixture label on top of another label or a card', () => {
		const { nodes, edges } = layout(entities, relations);
		const at = new Map(nodes.map((n) => [n.id, n]));
		const handle = (id: string, handleId: string) => {
			const n = at.get(id)!;
			const side = handleId.split('-')[0] as Side;
			const x = side === 'left' ? n.position.x - HANDLE_OUT : n.position.x + NODE_W + HANDLE_OUT;
			return { side, p: { x, y: n.position.y + n.height! / 2 } };
		};
		const boxes: Rect[] = edges.map((e) => {
			const s = handle(e.source, e.sourceHandle!);
			const t = handle(e.target, e.targetHandle!);
			const { w, h } = labelSize(e.data!.relation.predicate, e.data!.relation.cardinality);
			const p = routeAt(route(s.p, s.side, t.p, t.side, e.data!.via), e.data!.label.t);
			return { x: p.x + e.data!.label.dx - w / 2, y: p.y + e.data!.label.dy - h / 2, w, h };
		});
		const cards: Rect[] = nodes.map((n) => ({ x: n.position.x, y: n.position.y, w: NODE_W, h: n.height! }));
		expect(boxes).toHaveLength(relations.length);
		for (let i = 0; i < boxes.length; i++) {
			for (let j = i + 1; j < boxes.length; j++)
				expect(overlaps(boxes[i]!, boxes[j]!), `${edges[i]!.id} vs ${edges[j]!.id}`).toBe(false);
			for (const [k, c] of cards.entries())
				expect(overlaps(boxes[i]!, c), `${edges[i]!.id} over ${nodes[k]!.id}`).toBe(false);
		}
	});

	it("threads a link that skips a column through a gap between that column's cards", () => {
		const { nodes, edges } = layout(entities, relations);
		const skip = edges.filter((e) => e.data!.via);
		expect(skip.map((e) => `${e.source}>${e.target}`)).toEqual(['Policy>ReturnAuthorization']);
		const middle = nodes.filter((n) => n.position.x === COLUMN_X.industry);
		const v = skip[0]!.data!.via!;
		expect(v.x1).toBeLessThan(COLUMN_X.industry);
		expect(v.x2).toBeGreaterThan(COLUMN_X.industry + NODE_W);
		for (const n of middle)
			expect(v.y < n.position.y || v.y > n.position.y + n.height!, `via inside ${n.id}`).toBe(true);
		expect(edges.filter((e) => !e.data!.via).every((e) => e.data!.via === null)).toBe(true);
	});

	it('nudges a label off its midpoint when the midpoint is taken', () => {
		const c = curve({ x: 0, y: 0 }, 'right', { x: 200, y: 0 }, 'left');
		const [a, b] = placeLabels(
			[
				{ route: [c], w: 60, h: 18 },
				{ route: [c], w: 60, h: 18 },
			],
			[],
		);
		expect(a).toEqual({ t: 0.5, dx: 0, dy: 0 });
		expect(b!.t !== 0.5 || b!.dy !== 0).toBe(true);
	});
});

describe('entity form validation', () => {
	const ok = {
		name: 'Invoice',
		label: 'Invoice',
		scope: 'industry' as const,
		industry: 'retail',
		department: null,
		attributes: [{ name: 'amount', data_type: 'money' }],
	};
	it('accepts a complete entity', () => {
		expect(entityFormErrors(ok)).toEqual([]);
	});
	it('mirrors the server: name, label, no "/", attribute name and type', () => {
		expect(entityFormErrors({ ...ok, name: ' ' })).toContain('Name is required.');
		expect(entityFormErrors({ ...ok, name: 'a/b' })).toContain('Name cannot contain /.');
		expect(entityFormErrors({ ...ok, label: '' })).toContain('Label is required.');
		expect(entityFormErrors({ ...ok, attributes: [{ name: 'x', data_type: '' }] })).toContain(
			'Every attribute needs a name and a type.',
		);
	});
	it('rejects duplicate attributes and a scope without its industry / department', () => {
		expect(
			entityFormErrors({
				...ok,
				attributes: [
					{ name: 'a', data_type: 'string' },
					{ name: 'a', data_type: 'string' },
				],
			}),
		).toContain('Attribute names must be unique.');
		expect(entityFormErrors({ ...ok, industry: null })).toContain('Industry is required for this scope.');
		expect(entityFormErrors({ ...ok, scope: 'industry_dept' })).toContain(
			'Department is required for a specialized entity.',
		);
		expect(entityFormErrors({ ...ok, scope: 'generic', industry: null })).toEqual([]);
	});
});

describe('relationFormErrors', () => {
	const ents = [ent('Customer', 'generic'), ent('Order', 'industry')];
	const existing = [rel('Customer', 'Order', 'places')];
	const ok = { subject: 'Customer', predicate: 'cancels', object: 'Order', cardinality: '1:n' };

	it('accepts a new lowerCamel relation between known entities', () => {
		expect(relationFormErrors(ok, ents, existing)).toEqual([]);
		expect(relationFormErrors({ ...ok, predicate: 'paidBy2' }, ents, existing)).toEqual([]);
	});
	it('rejects the predicates and cardinalities the server rejects', () => {
		for (const predicate of ['', 'Places', 'paid by', 'paid_by', '1st', 'a'.repeat(65)])
			expect(relationFormErrors({ ...ok, predicate }, ents, existing).join(' ')).toContain('Predicate');
		for (const cardinality of ['', '1:many', 'N:M'])
			expect(relationFormErrors({ ...ok, cardinality }, ents, existing)).toEqual(['Pick a cardinality.']);
	});
	it('names which end is unknown or missing', () => {
		expect(relationFormErrors({ ...ok, subject: 'Nope' }, ents, existing)).toEqual([
			'Subject Nope is not an entity.',
		]);
		expect(relationFormErrors({ ...ok, object: '' }, ents, existing)).toEqual(['Pick an object.']);
	});
	it('flags an existing (subject, predicate, object) whatever the cardinality', () => {
		expect(relationFormErrors({ ...ok, predicate: 'places', cardinality: 'n:m' }, ents, existing)).toEqual([
			'Customer places Order already exists.',
		]);
	});
});
