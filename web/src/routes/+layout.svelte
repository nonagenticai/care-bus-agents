<script lang="ts">
	import '../app.css';
	import { env } from '$env/dynamic/public';
	import { page } from '$app/state';
	import { missingConfig, readConfig } from '$lib/config';
	import { getToken, initAuth, logout } from '$lib/auth/keycloak';
	import { session } from '$lib/auth/session.svelte';
	import { createClient } from '$lib/api/client';
	import { app, bootstrapScope, selectJob } from '$lib/app.svelte';
	import { MATTERS_ENABLED, RESEARCH_ENABLED } from '$lib/features';
	import { goto } from '$app/navigation';
	import { jobPath, switchJobPath } from '$lib/scope';
	import Icon from '$lib/ui/Icon.svelte';
	import Drawer from '$lib/ui/Drawer.svelte';
	import Toast from '$lib/ui/Toast.svelte';
	import TenantPicker from '$lib/ui/TenantPicker.svelte';
	import { tenantName } from '$lib/tenants';

	let { children } = $props();
	const missing = missingConfig(env);
	let authError = $state('');
	let navOpen = $state(false);
	let collapsed = $state(false);
	// Deployment labels come from runtime PUBLIC_* settings; never hardcode a job or environment.
	const environment = env.PUBLIC_CAREBUS_ENVIRONMENT || 'Environment not configured';
	// The current job (the URL's on job pages, else the one last picked); its label, else the deployment's label.
	const job = $derived(
		app.jobs.find((j) => j.key === app.job)?.label || env.PUBLIC_CAREBUS_JOB_LABEL || app.job,
	);
	/** The job picker: on a job page the same page of the other job, elsewhere (console, chat, demo) just the job. */
	function chooseJob(key: string) {
		const target = switchJobPath(page.url.pathname, key);
		if (target) void goto(target);
		else selectJob(key);
	}
	/** Pages remount when the tenant or job they read changes. */
	const scopeKey = $derived(`${app.activeTenant ?? ''}|${app.job}`);
	let tenantOpen = $state(false);
	const tenantLabel = $derived(
		tenantName(app.tenants, app.activeTenant ?? session.me?.active_tenant ?? null),
	);
	const roleLabel = $derived(
		session.superAdmin
			? 'Super admin'
			: session.can('tenant_admin')
				? 'Tenant admin'
				: session.can('tenant_user')
					? 'Tenant user'
					: 'No role',
	);
	function navigationToggle() {
		if (narrow) navOpen = !navOpen;
		else collapsed = !collapsed;
	}

	/** Job pages carry the current job in their URL (plan 14); tenant pages do not. */
	const J = (sub = '') => jobPath(app.job, sub);
	const NAV: [string, [string, string, string][]][] = $derived([
		[
			'Live',
			[
				['/console', 'Console', 'console'],
				['/chat', 'Customer chat', 'chat'],
			],
		],
		[
			'Overview',
			[
				[J(), 'Dashboard', 'dashboard'],
				[J('/analytics'), 'Analytics', 'analytics'],
			],
		],
		[
			'Runtime',
			[
				[J('/agents'), 'Agents', 'agents'],
				[J('/buses'), 'Buses & topics', 'buses'],
				[J('/cases'), 'Cases', 'cases'],
				[J('/tasks'), 'Tasks', 'jobs'],
				...(RESEARCH_ENABLED ? ([[J('/research'), 'Research', 'search']] as [string, string, string][]) : []),
				...(MATTERS_ENABLED
					? ([[J('/matters'), 'Legal matters', 'cases']] as [string, string, string][])
					: []),
			],
		],
		[
			'Knowledge',
			[
				[J('/ontology'), 'Ontology', 'ontology'],
				[J('/procedures'), 'Procedures', 'procedures'],
				[J('/knowledge'), 'Sources', 'knowledge'],
				['/admin/knowledge/stores', 'Stores', 'stores'],
				['/admin/knowledge/jobs', 'Ingest jobs', 'jobs'],
				['/admin/knowledge/eval', 'RAG eval', 'analytics'],
			],
		],
		[
			'Integrations',
			[
				[J('/tools'), 'MCP tools', 'tools'],
				[J('/backends'), 'Backend systems', 'backends'],
				[J('/gates'), 'Laya gates', 'audit'],
				['/admin/providers', 'Providers', 'models'],
			],
		],
		[
			'Release',
			[
				[J('/refinements'), 'Base & refinements', 'procedures'],
				[J('/release'), 'Release & conflicts', 'audit'],
			],
		],
		['Demo', [['/demo', 'Demo data', 'demo']]],
		['Governance', [['/admin/audit', 'Audit log', 'audit']]],
		// super_admin only: the early-access signup queue (routes::signups answers 403 to everyone else).
		...(session.superAdmin
			? ([['Platform', [['/admin/onboarding', 'Onboarding', 'onboarding']]]] as [
					string,
					[string, string, string][],
				][])
			: []),
	]);
	/** Exact match for section roots that have sub-pages listed beside them in the nav. */
	const EXACT = $derived([J(), J('/knowledge')]);
	const isOn = (href: string) => {
		const path: string = page.url.pathname;
		return EXACT.includes(href) ? path === href : path === href || path.startsWith(`${href}/`);
	};
	/** Document title from the nav label of the current page (longest matching href wins). */
	const pageLabel = $derived.by(() => {
		const path: string = page.url.pathname;
		let best: [string, string] | undefined;
		for (const [, items] of NAV)
			for (const [href, label] of items)
				if ((path === href || path.startsWith(`${href}/`)) && href.length > (best?.[0].length ?? -1))
					best = [href, label];
		return best?.[1];
	});

	/** Below this width the nav is an off-canvas drawer; keep in sync with app.css. */
	const PHONE = '(max-width: 860px)';
	let narrow = $state(false);
	let dark = $state(false);
	const THEME_KEY = 'carebus-theme';
	$effect(() => {
		const mq = matchMedia(PHONE);
		const onChange = () => {
			narrow = mq.matches;
			if (!narrow) navOpen = false;
		};
		onChange();
		mq.addEventListener('change', onChange);
		const scheme = matchMedia('(prefers-color-scheme: dark)');
		const syncDark = () =>
			(dark = document.documentElement.dataset.theme
				? document.documentElement.dataset.theme === 'dark'
				: scheme.matches);
		syncDark();
		scheme.addEventListener('change', syncDark);
		return () => {
			mq.removeEventListener('change', onChange);
			scheme.removeEventListener('change', syncDark);
		};
	});

	if (!missing.length) {
		const cfg = readConfig(env);
		initAuth(cfg)
			.then(async (user) => {
				const client = createClient({
					baseUrl: cfg.apiUrl,
					token: getToken,
					job: () => app.job,
					activeTenant: () => app.activeTenant,
				});
				app.api = client;
				await bootstrapScope(client, user);
				app.ready = true;
			})
			.catch((e) => (authError = `Sign-in failed: ${(e as Error).message}`));
	}

	function toggleTheme() {
		const root = document.documentElement;
		const isDark = root.dataset.theme
			? root.dataset.theme === 'dark'
			: matchMedia('(prefers-color-scheme: dark)').matches;
		const next = isDark ? 'light' : 'dark';
		root.dataset.theme = next;
		dark = next === 'dark';
		try {
			localStorage.setItem(THEME_KEY, next);
		} catch {
			// Storage blocked (private mode, policy): the choice lasts for this page only.
		}
	}
</script>

<svelte:head><title>{pageLabel ? `${pageLabel} · Care Bus` : 'Care Bus'}</title></svelte:head>

{#snippet navigation()}
	<nav class="nav">
		{#each NAV as [group, items]}
			<div class="nav-group">
				<span>{group}</span>
				{#each items as [href, label, icon]}
					<a
						{href}
						class:on={isOn(href)}
						aria-current={isOn(href) ? 'page' : undefined}
						onclick={() => (navOpen = false)}><Icon name={icon} /><span>{label}</span></a
					>
				{/each}
			</div>
		{/each}
	</nav>
{/snippet}

{#if missing.length}
	<main class="content">
		<div class="view">
			<div class="callout err">
				<strong>This deployment is not configured</strong>
				<span
					>Missing runtime settings: {missing.join(', ')}. Set them in the pod's environment and restart; no
					rebuild is needed.</span
				>
			</div>
		</div>
	</main>
{:else if authError}
	<main class="content">
		<div class="view">
			<div class="callout err"><strong>Sign-in failed</strong><span>{authError}</span></div>
		</div>
	</main>
{:else if !app.ready}
	<main class="content"><div class="view"><p class="muted">Signing in…</p></div></main>
{:else if app.blocked === 'not_provisioned'}
	<main class="content">
		<div class="view">
			<div class="callout err" data-testid="not-provisioned">
				<strong>Your account has no care-bus tenant yet</strong>
				<span
					>You are signed in as {session.user?.name || session.me?.actor}, but care-bus has no tenant for this
					account. Ask your organisation's administrator to add you, then sign in again.</span
				>
			</div>
			<button class="btn" type="button" onclick={() => logout()}>Sign out</button>
		</div>
	</main>
{:else if app.blocked === 'tenant_suspended'}
	<main class="content">
		<div class="view">
			<div class="callout err">
				<strong>Your tenant is suspended</strong>
				<span
					>Care-bus is switched off for {session.me?.tenant?.name ?? 'your tenant'}. Contact your
					administrator.</span
				>
			</div>
			<button class="btn" type="button" onclick={() => logout()}>Sign out</button>
		</div>
	</main>
{:else if app.blocked === 'pick_tenant'}
	<main class="content">
		<div class="view">
			<div class="callout info">
				<strong>Choose a tenant to act in</strong>
				<span
					>You are a super admin without a tenant of your own. Every page acts in the tenant you name here.</span
				>
			</div>
			<TenantPicker />
			<button class="btn" type="button" onclick={() => logout()}>Sign out</button>
		</div>
	</main>
{:else}
	<div class="shell" class:nav-collapsed={collapsed && !narrow}>
		{#if !narrow}
			<aside class="side" id="main-navigation" aria-label="Main navigation">
				<div class="brand">
					<span class="brand-mark" aria-hidden="true"><i></i><i></i><i></i></span>
					<div><b>Care Bus</b><span>{job}</span></div>
				</div>
				{@render navigation()}
			</aside>
		{/if}
		<div class="main">
			<div class="topbar">
				<button
					class="icon-btn menu-btn"
					type="button"
					aria-label={narrow ? 'Open navigation' : collapsed ? 'Show navigation' : 'Hide navigation'}
					aria-expanded={narrow ? navOpen : !collapsed}
					aria-controls={narrow ? 'mobile-navigation' : 'main-navigation'}
					onclick={navigationToggle}><Icon name="menu" /></button
				>
				<span class="deployment">{environment} · {job}</span>
				{#if app.jobs.length > 1}
					<select
						class="input job-pick"
						aria-label="Job"
						value={app.job}
						onchange={(e) => chooseJob(e.currentTarget.value)}
					>
						{#each app.jobs as j (j.key)}<option value={j.key}>{j.label}</option>{/each}
					</select>
				{/if}
				<div class="who">
					<button
						class="icon-btn"
						type="button"
						aria-label={dark ? 'Switch to light mode' : 'Switch to dark mode'}
						onclick={toggleTheme}><Icon name={dark ? 'sun' : 'moon'} /></button
					>
					{#if session.superAdmin}
						<button
							class="btn sm"
							type="button"
							title="The tenant every page acts in (X-Active-Tenant-Id)"
							onclick={() => (tenantOpen = true)}>Tenant: {tenantLabel}</button
						>
					{/if}
					<span>{session.user?.name} · {roleLabel}</span>
					<button class="btn sm" type="button" onclick={() => logout()}>Sign out</button>
				</div>
			</div>
			<main class="content">
				<div class="view">
					{#key scopeKey}{@render children()}{/key}
				</div>
			</main>
		</div>
	</div>
	{#if session.superAdmin}
		<Drawer bind:open={tenantOpen} title="Act in tenant">
			{#if tenantOpen}<TenantPicker onpicked={() => (tenantOpen = false)} />{/if}
		</Drawer>
	{/if}
	{#if narrow}
		<Drawer bind:open={navOpen} title="Navigation">
			<div id="mobile-navigation">{@render navigation()}</div>
		</Drawer>
	{/if}
{/if}
<Toast />

<style>
	.job-pick {
		width: auto;
		max-width: 24ch;
	}
</style>
