// SPDX-License-Identifier: Apache-2.0
/** The dark-terminal identity (matches agentd.dev). One place to touch. */
export const theme = {
  accent: 'green',
  dim: 'gray',
  user: 'cyan',
  agent: 'green',
  command: 'yellow',
  error: 'red',
  warn: 'yellow',
  info: 'gray',
  border: 'gray',
} as const;

/** Spinner frames (no dependency needed for one spinner). */
export const SPINNER = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

/** Short state labels for task states. */
export function stateLabel(state: string): { label: string; color: string } {
  switch (state) {
    case 'TASK_STATE_SUBMITTED':
      return { label: 'queued', color: theme.dim };
    case 'TASK_STATE_WORKING':
      return { label: 'working', color: theme.accent };
    case 'TASK_STATE_INPUT_REQUIRED':
      return { label: 'needs input', color: theme.warn };
    case 'TASK_STATE_COMPLETED':
      return { label: 'done', color: theme.agent };
    case 'TASK_STATE_FAILED':
      return { label: 'failed', color: theme.error };
    case 'TASK_STATE_CANCELED':
      return { label: 'canceled', color: theme.dim };
    case 'TASK_STATE_REJECTED':
      return { label: 'rejected', color: theme.error };
    default:
      return { label: state, color: theme.dim };
  }
}

export function shortId(id: string, n = 10): string {
  return id.length <= n ? id : `${id.slice(0, n)}…`;
}

export function ago(ts: number): string {
  const s = Math.max(0, Math.floor((Date.now() - ts) / 1000));
  if (s < 60) return `${s}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m`;
  return `${Math.floor(s / 3600)}h`;
}
