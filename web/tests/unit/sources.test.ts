import { readFileSync } from 'node:fs';
import { describe, expect, it } from 'vitest';
import type { IngestJob, OntologyEntity, Paged, Source, SourceRun, StoreNode } from '../../src/lib/api/types';
import {
	attributeOptions,
	childrenSummary,
	deleteMessage,
	draftErrors,
	draftOf,
	euros,
	inputOf,
	jobCost,
	jobSourceLabel,
	leafStores,
	mapAllFields,
	replaceSummary,
	resyncToast,
	storeTree,
	testErrorText,
	testPlan,
	unmappedCount,
} from '../../src/lib/sources';

const fx = <T>(name: string): T => JSON.parse(readFileSync(`src/lib/api/fixtures/${name}.json`, 'utf8'));

describe('resync toast', () => {
	const run = fx<SourceRun>('source_run');
	it('reports what the returned run says, not that it started', () => {
		expect(resyncToast({ ...run, status: 'running', error: null })).toBe('Re-sync started.');
		expect(resyncToast({ ...run, status: 'unsupported', error: 'nope' })).toBe(
			'Not supported by strata_ingest yet: nope',
		);
		expect(resyncToast({ ...run, status: 'failed', error: 'strata_ingest refused it' })).toBe(
			'Re-sync failed: strata_ingest refused it',
		);
		expect(resyncToast({ ...run, status: 'failed', error: null })).toBe('Re-sync failed.');
		expect(resyncToast({ ...run, status: 'completed', error: null })).toBe('Re-sync completed.');
	});
});

describe('source drafts', () => {
	const api = fx<Source>('source');

	it('never prefills the secret and keeps it unless a new one is typed or removal is asked', () => {
		const d = draftOf(api);
		expect(d.type).toBe('api');
		expect(d.authType).toBe('header');
		expect(d.headerName).toBe('X-Api-Key');
		expect(d.secret).toBe('');
		expect('credentials' in inputOf(d, true)).toBe(false);
		expect(inputOf({ ...d, secret: 'new-key' }, true).credentials).toEqual({ secret: 'new-key' });
		expect(inputOf({ ...d, clearSecret: true }, true).credentials).toBeNull();
		expect(inputOf({ ...d, authType: 'none' }, true).credentials).toBeNull();
	});

	it('switching to a website drops mapping and clears stored credentials', () => {
		const d = {
			...draftOf(api),
			type: 'website' as const,
			urls: ' https://a.example.nl/x \n\nhttps://b.example.nl/y ',
		};
		const input = inputOf(d, true);
		expect(input.config).toEqual({ urls: ['https://a.example.nl/x', 'https://b.example.nl/y'] });
		expect(input.mapping).toEqual([]);
		expect(input.credentials).toBeNull();
	});

	it('a datatable keeps its first row as data only when the header box is unticked', () => {
		const dt = { ...api, type: 'datatable' as const, config: { ...api.config, format: 'csv' } };
		const d = draftOf(dt);
		expect(d.headerRow).toBe(true);
		expect('header_row' in inputOf(d, false).config).toBe(false);
		expect(inputOf({ ...d, headerRow: false }, false).config.header_row).toBe(false);
		const saved = draftOf({ ...dt, config: { ...dt.config, header_row: false } });
		expect(saved.headerRow).toBe(false);
		expect(inputOf({ ...saved, type: 'api' }, false).config).not.toHaveProperty('header_row');
	});

	it('round-trips an api source config and mapping', () => {
		const input = inputOf(draftOf(api), false);
		expect(input.config).toEqual(api.config);
		expect(input.mapping).toEqual(api.mapping);
		expect(input.schedule).toBe(api.schedule);
		expect(input.target_store_id).toBe(api.target_store_id);
	});

	it('names what is missing before anything is sent', () => {
		const errs = draftErrors(draftOf(null));
		expect(errs).toContain('Give the source a name.');
		expect(errs).toContain('Pick the store the documents go into.');
		expect(errs).toContain('Add at least one URL, one per line.');
		const bad = {
			...draftOf(api),
			schedule: 'daily',
			mapping: [{ source_field: 'a', entity: '', attribute: '' }],
		};
		expect(draftErrors(bad)).toEqual([
			'A schedule has five cron fields, for example 0 3 * * *.',
			'Pick an ontology attribute for every mapped field.',
		]);
	});
});

describe('helpers', () => {
	it('builds the store tree depth-first and lists only leaves as targets', () => {
		const stores = fx<Paged<StoreNode>>('knowledge_stores').items;
		const tree = storeTree(stores);
		expect(tree.map((t) => [t.node.name, t.depth])).toEqual([
			['care', 0],
			['returns', 1],
		]);
		expect(leafStores(stores).map((s) => s.name)).toEqual(['returns']);
		const orphan = { ...stores[1]!, id: 'x', parent_id: 'missing' };
		expect(storeTree([orphan]).map((t) => t.depth)).toEqual([0]);
	});

	it('shows costs in euros and unknown cost as a dash, never 0', () => {
		expect(euros(null)).toBe('—');
		expect(euros(0)).toBe('€0.00');
		expect(euros(1840)).toBe('€0.0018');
		expect(euros(2_500_000)).toBe('€2.50');
		const job = fx<IngestJob>('ingest_job');
		expect(jobCost(job)).toBe(1840);
		expect(jobCost({ ...job, steps: job.steps.map((s) => ({ ...s, cost_micros: null })) })).toBeNull();
		expect(jobSourceLabel(job)).toBe('https://help.example.nl/returns');
	});

	it('lists visible ontology attributes as Entity.attribute and marks PII', () => {
		const opts = attributeOptions(fx<Paged<OntologyEntity>>('ontology_entities').items);
		expect(opts.some((o) => o.label === 'Customer.customer_id')).toBe(true);
		expect(opts.some((o) => o.label === 'Customer.email (PII)')).toBe(true);
	});

	it('summarises a replace in one line', () => {
		expect(replaceSummary(null)).toBe('');
		expect(replaceSummary({ deleted: 2, already_gone: 0, kept_shared: 1, failed: ['d1'] })).toBe(
			'Replaced 2 older documents · 1 unchanged kept · 1 could not be deleted (retried on the next completed run)',
		);
	});
});

describe('delete and header rules', () => {
	const base = { id: 'x', purged: false, documents_deleted: 0, documents_kept: 0 };
	it('words the delete outcome from the response', () => {
		expect(deleteMessage({ ...base, documents_kept: 3 })).toBe(
			'Source deleted. 3 documents kept in strata_rag.',
		);
		expect(deleteMessage({ ...base, documents_kept: 1 })).toBe(
			'Source deleted. 1 document kept in strata_rag.',
		);
		expect(deleteMessage({ ...base, purged: true, documents_deleted: 2 })).toBe(
			'Source deleted with its 2 documents.',
		);
	});
	it('accepts the server header token rule and refuses what it refuses', () => {
		const d = { ...draftOf(fx<Source>('source')), authType: 'header' as const };
		const ok = (h: string) =>
			!draftErrors({ ...d, headerName: h }).some((e) => e.startsWith('Name a valid header'));
		expect(ok('X_Api_Key')).toBe(true);
		expect(ok('X-Api-Key')).toBe(true);
		for (const bad of ['Host', 'cookie', 'Transfer-Encoding', 'Te', '', 'a b', 'x'.repeat(65)])
			expect(ok(bad)).toBe(false);
	});
});

describe('test connection plan', () => {
	const saved = fx<Source>('source');
	const fresh = {
		...draftOf(null),
		type: 'api' as const,
		name: 'Users',
		url: 'https://jsonplaceholder.typicode.com/users',
	};

	it('an unsaved draft is tested as is: no name or store needed, nothing saved', () => {
		const p = testPlan({ ...fresh, name: '' }, null);
		expect(p).toEqual({
			kind: 'unsaved',
			body: {
				type: 'api',
				config: { url: 'https://jsonplaceholder.typicode.com/users', auth: { type: 'none' } },
			},
			note: 'Tests the settings above. Nothing is saved.',
		});
	});

	it('an unsaved draft sends a typed secret only when its auth uses one', () => {
		const bearer = testPlan({ ...fresh, authType: 'bearer', secret: 'typed-now' }, null);
		expect(bearer.kind === 'unsaved' && bearer.body.credentials).toEqual({ secret: 'typed-now' });
		const none = testPlan({ ...fresh, authType: 'none', secret: 'left-over' }, null);
		expect(none.kind === 'unsaved' && 'credentials' in none.body).toBe(false);
		const header = testPlan({ ...fresh, authType: 'header', headerName: ' X-Key ', secret: 'k' }, null);
		expect(header.kind === 'unsaved' && header.body.config.auth).toEqual({
			type: 'header',
			header_name: 'X-Key',
		});
	});

	it('names every connection blocker, including a key typed into the settings', () => {
		const p = testPlan({ ...fresh, url: 'ftp://x', authType: 'header', headerName: 'Host' }, null, {
			secretInSettings: true,
		});
		expect(p.kind === 'blocked' && p.reasons).toEqual([
			'The URL must start with http:// or https://.',
			'Name a valid header, for example X-Api-Key (not Host or Cookie).',
			'Move the key out of the URL or header name into Credentials.',
		]);
	});

	it('a saved, unchanged source is tested as stored; edits go along as an override, never saved', () => {
		expect(testPlan(draftOf(saved), saved)).toEqual({ kind: 'saved', id: saved.id, body: null, note: '' });
		// a store or name edit does not matter to a test
		expect(testPlan({ ...draftOf(saved), targetStoreId: '', name: 'x' }, saved)).toMatchObject({
			body: null,
		});
		const path = testPlan({ ...draftOf(saved), url: 'https://oms.example.com/v3/customers' }, saved);
		expect(path).toEqual({
			kind: 'saved',
			id: saved.id,
			body: {
				config: {
					url: 'https://oms.example.com/v3/customers',
					auth: { type: 'header', header_name: 'X-Api-Key' },
				},
			},
			note: 'Tests your edits without saving them.',
		});
		const typed = testPlan({ ...draftOf(saved), secret: 'typed-now' }, saved);
		expect(typed.kind === 'saved' && typed.body).toEqual({ credentials: { secret: 'typed-now' } });
		const cleared = testPlan({ ...draftOf(saved), clearSecret: true }, saved);
		expect(cleared.kind === 'saved' && cleared.body).toEqual({ credentials: null });
	});

	it('the stored secret is never aimed at a different host: a new host needs the secret again', () => {
		const moved = { ...draftOf(saved), url: 'https://elsewhere.example.nl/customers' };
		const p = testPlan(moved, saved);
		expect(p.kind === 'blocked' && p.reasons).toEqual([
			'Enter the secret again to test a different host; the stored one is only sent to the saved host.',
		]);
		const withSecret = testPlan({ ...moved, secret: 'typed-now' }, saved);
		expect(withSecret.kind === 'saved' && withSecret.body?.credentials).toEqual({ secret: 'typed-now' });
		expect(testPlan({ ...moved, authType: 'none' }, saved).kind).toBe('saved');
	});

	it('words a failed test request', () => {
		expect(testErrorText({ status: 403, code: 'forbidden', message: 'x' })).toBe(
			'You need the tenant_admin role to test a source.',
		);
		expect(testErrorText({ status: 0, code: 'unavailable', message: 'x' })).toBe(
			'carebus-api is unreachable. Nothing was tested.',
		);
		expect(testErrorText({ status: 400, code: 'address_refused', message: '10.0.0.1 is private' })).toBe(
			'The address is refused (private, loopback or internal hosts are blocked): 10.0.0.1 is private',
		);
		expect(testErrorText({ status: 502, code: 'bad_gateway', message: 'upstream timed out' })).toBe(
			'upstream timed out',
		);
		expect(testErrorText(new Error(''))).toBe('The test failed.');
	});
});

describe('map all discovered fields', () => {
	const opts = [
		{ entity: 'Customer', attribute: 'email' },
		{ entity: 'Customer', attribute: 'name' },
		{ entity: 'Order', attribute: 'name' },
		{ entity: 'Order', attribute: 'id' },
		{ entity: 'Customer', attribute: 'id' },
	];

	it('adds a row per unmapped field and matches attribute names case-insensitively', () => {
		const rows = [{ source_field: 'Phone', entity: 'Customer', attribute: 'phone' }];
		const r = mapAllFields(['phone', 'Email', 'website'], rows, opts);
		expect(r.rows).toEqual([
			rows[0],
			{ source_field: 'Email', entity: 'Customer', attribute: 'email' },
			{ source_field: 'website', entity: '', attribute: '' },
		]);
		expect(r).toMatchObject({ added: 2, matched: 1 });
	});

	it('an ambiguous name goes to the entity already in use, else is left to pick', () => {
		const used = [{ source_field: 'mail', entity: 'Customer', attribute: 'email' }];
		expect(mapAllFields(['name'], used, opts).rows[1]).toEqual({
			source_field: 'name',
			entity: 'Customer',
			attribute: 'name',
		});
		expect(mapAllFields(['name'], [], opts).rows[0]).toEqual({
			source_field: 'name',
			entity: '',
			attribute: '',
		});
	});

	it('does not add a field twice and counts what is left to map', () => {
		expect(mapAllFields(['id', 'ID'], [], opts).added).toBe(1);
		expect(
			unmappedCount(['id', 'email', 'name'], [{ source_field: 'EMAIL', entity: '', attribute: '' }]),
		).toBe(2);
		expect(unmappedCount([], [])).toBe(0);
	});
});

describe('website crawl and sitemap modes', () => {
	const site = fx<Source>('source');
	const web = (config: Record<string, unknown>): Source => ({
		...site,
		type: 'website',
		mapping: [],
		has_credentials: false,
		config,
	});
	const store = { targetStoreId: 'store-1', name: 'Help centre' };

	it('a config without a mode is listed pages and saves exactly as before', () => {
		const d = draftOf(web({ urls: ['https://help.example.nl/a'] }));
		expect(d.mode).toBe('urls');
		expect(inputOf(d, false).config).toEqual({ urls: ['https://help.example.nl/a'] });
	});

	it('crawl sends one start url and only the settings that were filled in', () => {
		const d = {
			...draftOf(web({ urls: [] })),
			...store,
			mode: 'crawl' as const,
			url: ' https://help.example.nl/ ',
		};
		expect(inputOf(d, false).config).toEqual({ mode: 'crawl', url: 'https://help.example.nl/' });
		const full = {
			...d,
			maxDepth: 3,
			maxPages: 80,
			allowedDomains: 'help.example.nl\n\n docs.example.nl ',
			linkQuery: ' returns ',
		};
		const config = inputOf(full, false).config;
		expect(config).toEqual({
			mode: 'crawl',
			url: 'https://help.example.nl/',
			crawl: {
				max_depth: 3,
				max_pages: 80,
				allowed_domains: ['help.example.nl', 'docs.example.nl'],
				link_query: 'returns',
			},
		});
		expect(draftErrors(full)).toEqual([]);
		const back = draftOf(web(config));
		expect(inputOf(back, false).config).toEqual(config);
	});

	it('sitemap sends its url and filters, never the crawl settings', () => {
		const d = {
			...draftOf(web({ urls: [] })),
			...store,
			mode: 'sitemap' as const,
			url: 'https://help.example.nl/sitemap.xml',
			maxDepth: 3,
			maxUrls: 300,
			include: '/help/',
			exclude: '/archive/\n?print',
		};
		const config = inputOf(d, false).config;
		expect(config).toEqual({
			mode: 'sitemap',
			url: 'https://help.example.nl/sitemap.xml',
			sitemap: { max_urls: 300, include: ['/help/'], exclude: ['/archive/', '?print'] },
		});
		expect(draftOf(web(config)).maxUrls).toBe(300);
	});

	it('names out-of-range settings before anything is sent', () => {
		const base = { ...draftOf(web({ urls: [] })), ...store };
		expect(draftErrors({ ...base, mode: 'crawl', url: 'help.example.nl' })).toEqual([
			'The start page must start with http:// or https://.',
		]);
		const crawl = { ...base, mode: 'crawl' as const, url: 'https://help.example.nl/' };
		expect(draftErrors({ ...crawl, maxDepth: 6 })).toEqual(['Link depth is a whole number from 1 to 5.']);
		expect(draftErrors({ ...crawl, maxPages: 0 })).toEqual(['Max pages is a whole number from 1 to 200.']);
		expect(draftErrors({ ...crawl, allowedDomains: 'https://help.example.nl' })).toHaveLength(1);
		const sm = { ...base, mode: 'sitemap' as const, url: 'https://help.example.nl/sitemap.xml' };
		expect(draftErrors({ ...sm, maxUrls: 1001 })).toEqual(['Max URLs is a whole number from 1 to 1000.']);
		expect(draftErrors({ ...sm, exclude: 'x'.repeat(201) })).toEqual([
			'Exclude takes up to 50 URL fragments of at most 200 characters.',
		]);
		// listed pages ignore the start url and the crawl settings
		expect(draftErrors({ ...base, mode: 'urls', urls: 'https://help.example.nl/a', maxDepth: 9 })).toEqual(
			[],
		);
	});

	it('summarises a parent job and labels a crawl job by its start page', () => {
		expect(childrenSummary(undefined)).toBe('');
		expect(childrenSummary({ total: 12, by_status: { completed: 10, failed: 2, queued: 0 } })).toBe(
			'12 pages: 10 completed · 2 failed',
		);
		expect(childrenSummary({ total: 1, by_status: {} })).toBe('1 page');
		const job = fx<Paged<IngestJob>>('ingest_jobs').items[0]!;
		expect(
			jobSourceLabel({ ...job, source: { kind: 'crawl', config: { url: 'https://help.example.nl/' } } }),
		).toBe('crawl https://help.example.nl/');
	});
});
