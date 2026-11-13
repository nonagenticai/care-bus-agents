<script lang="ts" module>
	// Path data from design/admin/ui.js `P` (plus), and demo, console and chat (not in ui.js).
	export const PATHS: Record<string, string> = {
		dashboard: 'M4 4h7v7H4zM13 4h7v4h-7zM13 10h7v10h-7zM4 13h7v7H4z',
		analytics: 'M4 20V10M10 20V4M16 20v-7M22 20H2',
		agents: 'M8 8h8v8H8zM12 2v4M12 18v4M2 12h4M18 12h4',
		buses: 'M3 6h18M3 12h18M3 18h18M7 4v4M15 10v4M11 16v4',
		cases: 'M4 4h16v12H4zM4 16l3 4h10l3-4M9 10h6',
		ontology: 'M12 5a2 2 0 1 0 0-.01M5 19a2 2 0 1 0 0-.01M19 19a2 2 0 1 0 0-.01M12 7v4M12 11l-6 6M12 11l6 6',
		procedures: 'M5 4h5v5H5zM14 15h5v5h-5zM7.5 9v4.5h9V15',
		knowledge: 'M4 5a2 2 0 0 1 2-2h13v16H6a2 2 0 0 0-2 2zM4 21V5M8 7h7',
		stores: 'M12 3 3 7.5l9 4.5 9-4.5zM3 12l9 4.5 9-4.5M3 16.5 12 21l9-4.5',
		jobs: 'M4 5h16v4H4zM4 11h10v4H4zM4 17h6v4H4z',
		tools: 'M14.7 6.3a4 4 0 0 0-5.4 5.4L3 18l3 3 6.3-6.3a4 4 0 0 0 5.4-5.4l-2.5 2.5-2.4-.6-.6-2.4z',
		backends:
			'M4 6c0-1.7 3.6-3 8-3s8 1.3 8 3-3.6 3-8 3-8-1.3-8-3zM4 6v12c0 1.7 3.6 3 8 3s8-1.3 8-3V6M4 12c0 1.7 3.6 3 8 3s8-1.3 8-3',
		models: 'M12 3l2.4 5.6L20 11l-5.6 2.4L12 19l-2.4-5.6L4 11l5.6-2.4z',
		audit: 'M9 4h6l1 2h3v15H5V6h3zM9 12l2 2 4-4',
		demo: 'M4 4h16v16H4zM4 9h16M9 9v11',
		console: 'M4 5h16v11H8l-4 4z',
		chat: 'M3 6h12v8H7l-4 3zM15 9h6v8l-3-2.5h-6V14',
		search: 'M11 4a7 7 0 1 0 0 14 7 7 0 0 0 0-14zM21 21l-4.3-4.3',
		sun: 'M12 8a4 4 0 1 0 0 8 4 4 0 0 0 0-8zM12 2v2M12 20v2M4.9 4.9l1.4 1.4M17.7 17.7l1.4 1.4M2 12h2M20 12h2M4.9 19.1l1.4-1.4M17.7 6.3l1.4-1.4',
		moon: 'M20 14.5A8 8 0 1 1 9.5 4a6.5 6.5 0 0 0 10.5 10.5z',
		menu: 'M3 6h18M3 12h18M3 18h18',
		x: 'M6 6l12 12M18 6L6 18',
		plus: 'M12 5v14M5 12h14',
		arrow: 'M5 12h14M13 6l6 6-6 6',
		info: 'M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM12 11v5M12 7.5v.5',
		warn: 'M12 3 2 20h20zM12 10v4M12 17v.5',
		err: 'M12 3a9 9 0 1 0 0 18 9 9 0 0 0 0-18zM9 9l6 6M15 9l-6 6',
		onboarding: 'M9 11a4 4 0 1 0 0-8 4 4 0 0 0 0 8zM2 21a7 7 0 0 1 14 0M19 8v6M16 11h6',
	};
</script>

<script lang="ts">
	let { name, size = 18 }: { name: string; size?: number } = $props();
</script>

<svg
	class="ic"
	width={size}
	height={size}
	viewBox="0 0 24 24"
	fill="none"
	stroke="currentColor"
	stroke-width="1.7"
	stroke-linecap="round"
	stroke-linejoin="round"
	aria-hidden="true"><path d={PATHS[name] ?? PATHS.dashboard} /></svg
>
