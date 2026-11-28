/**
 * Neutral 24x24 glyphs (one filled `<path>` each) for the chat looks. Generic shapes only: no product logos or
 * brand marks of any chat app. Only the glyphs the live conversation draws: the decorative rail is gone.
 */
export const ICON = {
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
