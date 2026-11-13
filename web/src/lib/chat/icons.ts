/**
 * Neutral 24x24 glyphs (one filled `<path>` each) for the chat looks. Generic shapes only: no product logos or
 * brand marks of any chat app.
 */
export const ICON = {
	bell: 'M12 2a6 6 0 0 0-6 6v3l-2 3v1h16v-1l-2-3V8a6 6 0 0 0-6-6Zm0 20a2.5 2.5 0 0 0 2.45-2h-4.9A2.5 2.5 0 0 0 12 22Z',
	chat: 'M4 4h16v12H7l-3 3V4Z',
	people:
		'M9 11a3 3 0 1 0 0-6 3 3 0 0 0 0 6Zm8 0a2.5 2.5 0 1 0 0-5 2.5 2.5 0 0 0 0 5Zm-8 1.5c-3 0-6 1.5-6 4V19h9v-2.5c0-1.2.5-2.3 1.4-3.1-1.2-.6-2.7-.9-4.4-.9Zm8 .5c-.6 0-1.2.1-1.8.2 1 .8 1.8 1.9 1.8 3.3V19h6v-2.5c0-2-2.6-3.5-6-3.5Z',
	calendar:
		'M7 2v2H5a2 2 0 0 0-2 2v13a2 2 0 0 0 2 2h14a2 2 0 0 0 2-2V6a2 2 0 0 0-2-2h-2V2h-2v2H9V2H7ZM5 9h14v10H5V9Z',
	files: 'M6 2a2 2 0 0 0-2 2v16a2 2 0 0 0 2 2h12a2 2 0 0 0 2-2V8l-6-6H6Zm7 1.5L18.5 9H13V3.5Z',
	apps: 'M4 4h7v7H4V4Zm9 0h7v7h-7V4ZM4 13h7v7H4v-7Zm9 0h7v7h-7v-7Z',
	home: 'M12 3 3 10v11h6v-7h6v7h6V10L12 3Z',
	send: 'M3 11 21 3l-8 18-2.5-7L3 11Z',
	search:
		'M10 3a7 7 0 1 0 4.2 12.6l5.1 5.1 1.4-1.4-5.1-5.1A7 7 0 0 0 10 3Zm0 2a5 5 0 1 1 0 10 5 5 0 0 1 0-10Z',
	more: 'M12 5.5a1.75 1.75 0 1 0 0-3.5 1.75 1.75 0 0 0 0 3.5Zm0 8.25a1.75 1.75 0 1 0 0-3.5 1.75 1.75 0 0 0 0 3.5Zm0 8.25a1.75 1.75 0 1 0 0-3.5 1.75 1.75 0 0 0 0 3.5Z',
	check: 'm9 16.2-4.2-4.2-1.4 1.4L9 19 21 7l-1.4-1.4L9 16.2Z',
	checks: 'M1 12.6l1.4-1.4 4.6 4.6 9.6-9.6 1.4 1.4L7 18.6zM11.6 17.2l1.4 1.4 10.4-11-1.4-1.4z',
	clock:
		'M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20Zm0 2a8 8 0 1 1 0 16 8 8 0 0 1 0-16Zm-1 3v6l5 3 1-1.7-4-2.3V7h-2Z',
	alert: 'M12 2a10 10 0 1 0 0 20 10 10 0 0 0 0-20Zm-1 5h2v7h-2V7Zm0 9h2v2h-2v-2Z',
	lock: 'M12 2a5 5 0 0 0-5 5v3H5v12h14V10h-2V7a5 5 0 0 0-5-5Zm-3 8V7a3 3 0 1 1 6 0v3H9Z',
} as const;

/** The decorative activity rail of the Teams-style look (aria-hidden: none of it navigates). */
export const RAIL: { key: string; label: string; icon: string; active?: boolean }[] = [
	{ key: 'activity', label: 'Activity', icon: ICON.bell },
	{ key: 'chat', label: 'Chat', icon: ICON.chat, active: true },
	{ key: 'people', label: 'People', icon: ICON.people },
	{ key: 'calendar', label: 'Calendar', icon: ICON.calendar },
	{ key: 'files', label: 'Files', icon: ICON.files },
];
