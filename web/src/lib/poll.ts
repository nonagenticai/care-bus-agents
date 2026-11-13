import { ApiError } from './api/client';

/** The longest wait between two polls after failures. */
export const POLL_BACKOFF_MAX_MS = 30_000;
/** A poll that has failed this many times in a row stops (the page offers Retry). */
export const POLL_MAX_FAILURES = 6;

/**
 * What a poll does after a failed read:
 * - `gone`: 404, the thing it follows no longer exists (deleted, or another tenant is now active): stop.
 * - `auth`: 401 / 403, the session expired or the role was taken away: stop, polling again cannot help.
 * - `retry`: anything else (network, a restart, a 5xx) is not the outcome: wait `delay` and read again.
 * - `give_up`: `POLL_MAX_FAILURES` failures in a row: stop and let the person retry.
 */
export type PollStep =
	| { kind: 'gone' }
	| { kind: 'auth'; message: string }
	| { kind: 'retry'; delay: number }
	| { kind: 'give_up'; message: string };

/**
 * `failures` counts this one (1 for the first failure in a row). The wait starts at `base` and doubles per
 * failure, capped at `POLL_BACKOFF_MAX_MS`.
 */
export function afterPollError(e: unknown, failures: number, base: number): PollStep {
	if (e instanceof ApiError) {
		if (e.status === 404) return { kind: 'gone' };
		if (e.status === 401)
			return { kind: 'auth', message: 'Your session has expired. Sign in again to follow it.' };
		if (e.status === 403)
			return { kind: 'auth', message: 'You no longer have permission to read this. Ask a tenant admin.' };
	}
	if (failures >= POLL_MAX_FAILURES)
		return { kind: 'give_up', message: e instanceof Error ? e.message : String(e) };
	return { kind: 'retry', delay: Math.min(base * 2 ** (failures - 1), POLL_BACKOFF_MAX_MS) };
}
