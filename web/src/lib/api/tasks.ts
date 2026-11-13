// MCP tasks (`/api/jobs/{job}/mcp-tasks`): the staff inbox of long-running MCP work. Types mirror
// crates/carebus-api src/routes/mcp_tasks.rs (TaskView). The pure helpers below are what the tasks page renders:
// the answer form is generated from each question's JSON Schema and checked here before the POST (the server
// enforces the same rules).

export type TaskStatus = 'working' | 'input_required' | 'completed' | 'cancelled' | 'failed' | 'lost';
export type TaskOutcome = 'completed' | 'failed' | 'lost' | 'cancelled';
export type AnswerAction = 'accept' | 'decline' | 'cancel';

export type JsonSchema = {
	type?: string;
	required?: string[];
	properties?: Record<string, { type?: string; enum?: unknown[]; title?: string; description?: string }>;
};

export type TaskQuestion = { key: string; message: string; requested_schema: JsonSchema };
export type TaskResult = {
	outcome: TaskOutcome;
	ok: boolean;
	result?: unknown;
	error?: string;
	retryable?: boolean;
};
export type TaskView = {
	task_id: string;
	tool: string;
	status: TaskStatus;
	status_message: string | null;
	run_id: string | null;
	case_id: string | null;
	questions: TaskQuestion[];
	result: TaskResult | null;
	cancel_requested: boolean;
	created_by: string;
	created_at: string;
	updated_at: string;
	terminal_at: string | null;
};
export type TaskList = { items: TaskView[] };
export type AnswerBody = {
	key: string;
	action: AnswerAction;
	content?: Record<string, string>;
};
export type AnswerResult = { sent: boolean; already_answered: boolean; task: TaskView };
export type CancelResult = { sent: boolean; task: TaskView };
export type TaskListState = 'open' | 'all';

/** Open tasks are re-read this often while any exists. */
export const TASKS_POLL_MS = 5000;

export const TASK_STATUS_LABEL: Record<TaskStatus, string> = {
	working: 'Working',
	input_required: 'Waiting for an answer',
	completed: 'Completed',
	cancelled: 'Cancelled',
	failed: 'Failed',
	lost: 'Lost',
};
const TERMINAL: readonly TaskStatus[] = ['completed', 'cancelled', 'failed', 'lost'];
export const isTaskTerminal = (s: TaskStatus) => TERMINAL.includes(s);
export const taskStatusLabel = (s: string) => TASK_STATUS_LABEL[s as TaskStatus] ?? s;
/** The `Status` component word: waiting is shown as open (warn). */
export function taskStatusWord(s: string): string {
	return s === 'input_required' ? 'open' : s === 'lost' ? 'failed' : s;
}

export type QuestionField = {
	name: string;
	kind: 'choice' | 'text';
	options: string[];
	required: boolean;
};

/** Field order the form shows: decision first, then reason, comment, then the rest in schema order. */
const FIRST = ['decision', 'reason', 'comment'];

/** Ordered field descriptors of a requested_schema: a property with `enum` is a choice, anything else is text. */
export function questionFields(schema: JsonSchema | null | undefined): QuestionField[] {
	const props = schema?.properties ?? {};
	const required = new Set(schema?.required ?? []);
	const names = Object.keys(props);
	const rank = (n: string) => {
		const i = FIRST.indexOf(n);
		return i < 0 ? FIRST.length : i;
	};
	return names
		.map((name, i) => ({ name, i }))
		.sort((a, b) => rank(a.name) - rank(b.name) || a.i - b.i)
		.map(({ name }) => {
			const en = props[name]?.enum;
			const options = Array.isArray(en) ? en.map(String) : [];
			return {
				name,
				kind: options.length ? ('choice' as const) : ('text' as const),
				options,
				required: required.has(name),
			};
		});
}

/** The message to show for a wrong answer, or null when it can be sent. */
export function validateAnswer(
	schema: JsonSchema | null | undefined,
	content: Record<string, string | undefined>,
): string | null {
	const fields = questionFields(schema);
	const known = new Set(fields.map((f) => f.name));
	for (const k of Object.keys(content)) {
		if (!known.has(k)) return `Unknown field "${k}".`;
	}
	for (const f of fields) {
		const v = (content[f.name] ?? '').trim();
		if (f.required && !v) return `${f.name} is required.`;
		if (v && f.kind === 'choice' && !f.options.includes(v))
			return `${f.name} must be one of: ${f.options.join(', ')}.`;
	}
	if (content.decision === 'rejected' && !(content.reason ?? '').trim())
		return 'A reason is required when the decision is rejected.';
	return null;
}

/** The content to POST: blank optional fields dropped, values trimmed. */
export function answerContent(content: Record<string, string | undefined>): Record<string, string> {
	const out: Record<string, string> = {};
	for (const [k, v] of Object.entries(content)) {
		const t = (v ?? '').trim();
		if (t) out[k] = t;
	}
	return out;
}

/** The server's refusal of an answer, in words for the person. */
export function answerProblem(code: string, message: string): string {
	if (code === 'answered_elsewhere') return 'Someone else already answered this question.';
	if (code === 'not_waiting') return 'This task is no longer waiting for an answer.';
	if (code === 'task_lost') return 'This task was lost; it can no longer be answered.';
	if (code === 'task_finished') return 'This task has already finished.';
	return message;
}

export const when = (iso: string | null): string => (iso ? iso.replace('T', ' ').slice(0, 19) : '—');
