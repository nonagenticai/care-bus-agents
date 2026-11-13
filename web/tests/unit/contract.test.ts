// D2: plan 05's tests write these fixtures from its real Rust structs. This test fails when a
// fixture's keys differ from the key map, and shapes.ts fails to compile when a key map differs
// from types.ts. Together they keep types.ts honest. The hand-written e2e overlays in
// tests/e2e/fixtures/ are held to the same key maps so a mock cannot serve a shape the API never sends.
import { existsSync, readdirSync, readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import { EVAL_FIXTURES, FIXTURES, type Presence } from '../../src/lib/api/shapes';

const DIR = 'src/lib/api/fixtures';
const OVERLAYS = 'tests/e2e/fixtures';
const read = (dir: string, name: string) => JSON.parse(readFileSync(`${dir}/${name}.json`, 'utf8'));
const keys = (o: unknown) => Object.keys(o as object).sort();
const jsonNames = (dir: string) =>
	readdirSync(dir)
		.filter((f) => f.endsWith('.json'))
		.map((f) => f.slice(0, -5))
		.sort();

/** Every always-present key is there, and nothing outside the map is. Optional keys may be absent. */
function expectKeys(obj: unknown, map: Record<string, Presence>, label: string) {
	const got = keys(obj);
	const required = Object.keys(map)
		.filter((key) => map[key] === true)
		.sort();
	expect(
		required.filter((key) => !got.includes(key)),
		`${label}: missing keys`,
	).toEqual([]);
	expect(
		got.filter((key) => !(key in map)),
		`${label}: keys not in types.ts`,
	).toEqual([]);
	// skip_serializing_if = "Option::is_none": an optional key is absent when empty, never null.
	const o = obj as Record<string, unknown>;
	expect(
		got.filter((key) => map[key] === 'optional' && o[key] === null),
		`${label}: optional keys sent as null`,
	).toEqual([]);
}

// Walks a dotted path; arrays fan out so every element is checked, not only the first.
function at(root: unknown, path: string): unknown[] {
	let cur: unknown[] = [root];
	for (const key of path.split('.')) {
		cur = cur.flatMap((x) => (Array.isArray(x) ? x : [x])).map((x) => (x as Record<string, unknown>)?.[key]);
	}
	return cur.flatMap((x) => (Array.isArray(x) ? x : [x]));
}

function checkBody(name: string, body: any, dir: string, label: string, specs = FIXTURES) {
	const spec = specs[name]!;
	if (spec.paged) {
		expect(keys(body)).toEqual(['items', 'next']);
		expect(body.items.length, `${label} needs at least one item`).toBeGreaterThan(0);
	}
	const samples: unknown[] = spec.paged ? body.items : [body];
	for (const [i, sample] of samples.entries()) {
		const where = spec.paged ? `${label} items[${i}]` : label;
		if (spec.keys) expectKeys(sample, spec.keys, where);
		if (spec.columnsOf) {
			const columns: string[] = read(dir, spec.columnsOf).columns;
			expect(columns.length, `${spec.columnsOf}.json needs columns`).toBeGreaterThan(0);
			expect(keys(sample), `${where} vs ${spec.columnsOf}.json columns`).toEqual([...columns].sort());
		}
		for (const [path, nested] of Object.entries(spec.nested ?? {})) {
			const items = at(sample, path);
			expect(items.every(Boolean), `${where}: ${path} has an empty value`).toBe(true);
			for (const [j, item] of items.entries()) expectKeys(item, nested, `${where}: ${path}[${j}]`);
		}
		for (const [path, allowed] of Object.entries(spec.values ?? {})) {
			for (const v of at(sample, path)) expect(allowed, `${where}: ${path}=${String(v)}`).toContain(v);
		}
	}
	// An empty list proves nothing about its element shape: at least one sample must carry each path.
	for (const path of Object.keys(spec.nested ?? {})) {
		expect(
			samples.some((sample) => at(sample, path).length > 0),
			`${label}: ${path} needs a sample value`,
		).toBe(true);
	}
}

describe('carebus-api fixtures match types.ts', () => {
	it('has exactly one fixture per declared route response', () => {
		const files = jsonNames(DIR);
		expect(files).toEqual(Object.keys(FIXTURES).sort());
		expect(files).toHaveLength(46);
	});

	for (const name of Object.keys(FIXTURES)) {
		it(`${name}.json has the keys of its type`, () => checkBody(name, read(DIR, name), DIR, `${name}.json`));
	}

	it('the published agent reply carries case_id; the other messages omit the key', () => {
		const messages: Record<string, unknown>[] = read(DIR, 'conversation').messages;
		const withCase = messages.filter((m) => 'case_id' in m);
		expect(withCase.length).toBeGreaterThan(0);
		for (const m of withCase) expect(typeof m.case_id).toBe('string');
		expect(messages.some((m) => !('case_id' in m))).toBe(true);
	});
});

describe('e2e overlays match types.ts', () => {
	const overlays = existsSync(OVERLAYS) ? jsonNames(OVERLAYS) : [];
	it('every overlay names a declared fixture', () => {
		expect(overlays.filter((n) => !(n in FIXTURES))).toEqual([]);
	});
	for (const name of overlays.filter((n) => n in FIXTURES)) {
		it(`tests/e2e/fixtures/${name}.json has the keys of its type`, () =>
			checkBody(name, read(OVERLAYS, name), DIR, `overlay ${name}.json`));
	}
});

describe('carebus-api eval fixtures match types.ts', () => {
	const EVAL = `${DIR}/eval`;
	it('has exactly one fixture per eval response', () => {
		expect(jsonNames(EVAL)).toEqual(Object.keys(EVAL_FIXTURES).sort());
	});
	for (const name of Object.keys(EVAL_FIXTURES)) {
		it(`eval/${name}.json has the keys of its type`, () =>
			checkBody(name, read(EVAL, name), DIR, `eval/${name}.json`, EVAL_FIXTURES));
	}
	it('a listed or started run is a summary; a read completed run carries questions and results', () => {
		const listed = read(EVAL, 'runs').items[0];
		expect(['results', 'questions'].filter((k) => k in listed)).toEqual([]);
		expect(listed.question_count).toBeGreaterThan(0);
		const run = read(EVAL, 'run');
		expect(run.status).toBe('completed');
		expect(run.results.length).toBeGreaterThan(0);
		const started = read(EVAL, 'run_started');
		expect(started.status).toBe('running');
		expect(['results', 'questions'].filter((k) => k in started)).toEqual([]);
		expect([started.verdict, started.reports, started.finished_at]).toEqual([null, null, null]);
	});
});
