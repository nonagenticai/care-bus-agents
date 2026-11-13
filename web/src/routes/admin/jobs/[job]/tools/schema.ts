export interface SchemaField {
	name: string;
	type: string;
	required: boolean;
	description: string;
}

const obj = (v: unknown): Record<string, unknown> | null =>
	v !== null && typeof v === 'object' && !Array.isArray(v) ? (v as Record<string, unknown>) : null;

function typeOf(p: Record<string, unknown>): string {
	const t = p.type;
	if (Array.isArray(t)) return t.join(' | ');
	if (typeof t === 'string') {
		const items = obj(p.items);
		if (t === 'array' && items && typeof items.type === 'string') return `${items.type}[]`;
		return t;
	}
	if (Array.isArray(p.enum)) return 'enum';
	const alt = (p.anyOf ?? p.oneOf) as unknown;
	if (Array.isArray(alt)) return alt.map((a) => typeOf(obj(a) ?? {})).join(' | ');
	return '—';
}

/** Top-level properties of a JSON Schema object, required ones first. Not an object schema → []. */
export function schemaFields(schema: unknown): SchemaField[] {
	const s = obj(schema);
	const props = obj(s?.properties);
	if (!props) return [];
	const req = new Set(Array.isArray(s?.required) ? (s.required as unknown[]).map(String) : []);
	return Object.entries(props)
		.map(([name, raw]) => {
			const p = obj(raw) ?? {};
			const en = Array.isArray(p.enum) ? ` One of: ${(p.enum as unknown[]).map(String).join(', ')}.` : '';
			return {
				name,
				type: typeOf(p),
				required: req.has(name),
				description: ((typeof p.description === 'string' ? p.description : '') + en).trim(),
			};
		})
		.sort((a, b) => Number(b.required) - Number(a.required));
}
