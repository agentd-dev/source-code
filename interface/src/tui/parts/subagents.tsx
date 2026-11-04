// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The subagents screen: the live list (feed-driven), a selectable row, and a
 * drill-down detail view (`subagent.get` — instruction, result, attempts) with
 * a way back. The detail is an introspection read, offered only while the
 * daemon runs with `a2a.introspection.enabled` (the card or the feed's hello
 * says which — `introspectionOn`); without it the list still shows the
 * summary the feed carries.
 *
 * The detail view is also where a subagent is talked to and read: a message
 * field of its own (`subagent.send`) with the short exchange above it, and a
 * log pane (`subagent.log`) that tails what the child logged.
 */
import React from 'react';
import { Box, Text } from 'ink';
import { DAEMON_KEYS, inert } from '../../client/index.js';
import type { Json, MirrorState } from '../../client/index.js';
import { ago, theme } from '../theme.js';
import { MultilineInput, type EditState } from './input.js';

/** One turn of the exchange the detail view shows: what was sent, what came back, or why nothing did. */
export interface ExchangeItem {
  who: 'you' | 'child' | 'note';
  text: string;
}

/** Text a subagent or the daemon wrote, made inert line by line: its newlines kept, any other control character spelled out. */
export function inertLines(text: string): string {
  return text
    .split('\n')
    .map((l) => inert(l))
    .join('\n');
}

/** A reply's text: a string as it is, anything else as its JSON. */
export function replyText(v: Json | undefined): string {
  if (v === undefined || v === null) return '';
  return typeof v === 'string' ? v : JSON.stringify(v);
}

/**
 * Why a message to this subagent could not be taken, from its summary — or
 * `null` when it can be. Only a running warm worker or a running instance
 * child takes one; asking the daemon anyway would only bring its refusal back.
 */
export function cannotMessage(summary: { [k: string]: Json } | undefined): string | null {
  if (!summary) return null;
  const status = String(summary.status ?? '');
  if (status !== '' && status !== 'running') return `not running (${status}) — a message reaches a running subagent only`;
  if (summary.tier !== 'instance' && summary.mode !== undefined && summary.mode !== 'warm') {
    return `not a warm subagent (${String(summary.mode)}) — only a warm worker or an instance child takes messages`;
  }
  return null;
}

/** The fields a compact log line leaves out: every line carries them, and none of them says what happened. */
const LOG_CORRELATION = new Set(['ts', 'level', 'event', 'run_id', 'agent_id', 'agent_path', 'comp', 'pid', 'trace_id', 'span_id', 'parent_span_id', 'seq']);

/**
 * One of agentd's JSON log lines, compact: its time of day, level and event,
 * then the fields that are this event's own as `key=value`. A line that is
 * not JSON is shown as it is. Inert either way: a log line is text the
 * terminal must not act on.
 */
export function compactLogLine(raw: string): { time: string; level: string; event: string; rest: string } {
  let v: unknown;
  try {
    v = JSON.parse(raw);
  } catch {
    return { time: '', level: '', event: '', rest: inert(raw) };
  }
  if (!v || typeof v !== 'object' || Array.isArray(v)) return { time: '', level: '', event: '', rest: inert(raw) };
  const o = v as Record<string, unknown>;
  const ts = typeof o.ts === 'string' ? o.ts : '';
  const time = /T(\d\d:\d\d:\d\d)/.exec(ts)?.[1] ?? ts;
  const fields = Object.entries(o)
    .filter(([k]) => !LOG_CORRELATION.has(k))
    .map(([k, x]) => `${k}=${typeof x === 'string' ? x : JSON.stringify(x)}`)
    .join(' ');
  return {
    time: inert(time),
    level: inert(String(o.level ?? '')),
    event: inert(String(o.event ?? '')),
    rest: inert(fields),
  };
}

function levelColor(level: string): string | undefined {
  switch (level) {
    case 'error':
      return theme.error;
    case 'warn':
      return theme.warn;
    case 'debug':
    case 'trace':
      return theme.dim;
    default:
      return undefined;
  }
}

/**
 * The log pane: `rows` rows of the child's log, ending `scroll` lines above
 * the newest. Each line is one row, cut at the pane's width — a log line that
 * wrapped would make the pane as tall as its longest lines, and a narrow pane
 * garbled.
 */
export function SubagentLog({
  handle,
  lines,
  note,
  scroll,
  rows,
  width,
}: {
  handle: string;
  lines: readonly string[];
  note?: string;
  scroll: number;
  rows: number;
  width: number;
}): React.JSX.Element {
  const body = Math.max(1, rows - 2);
  const end = Math.max(0, lines.length - scroll);
  const shown = lines.slice(Math.max(0, end - body), end);
  return (
    <Box flexDirection="column" width={width}>
      <Text color={theme.accent} bold wrap="truncate-end">
        {`log ${handle}`}
        <Text color={theme.dim}>
          {scroll > 0 ? ` · ${scroll} newer below` : ' · following'}
          {` · ${lines.length} lines`}
        </Text>
      </Text>
      {shown.length === 0 ? (
        <Text color={theme.dim} wrap="truncate-end">
          {note ? inert(note) : 'no lines yet'}
        </Text>
      ) : (
        shown.map((raw, i) => {
          const l = compactLogLine(raw);
          return (
            <Text key={end - shown.length + i} wrap="truncate-end">
              <Text color={theme.dim}>{l.time ? `${l.time} ` : ''}</Text>
              <Text color={levelColor(l.level)}>{l.level ? `${l.level.padEnd(5)} ` : ''}</Text>
              <Text bold>{l.event ? `${l.event} ` : ''}</Text>
              <Text color={theme.dim}>{l.rest}</Text>
            </Text>
          );
        })
      )}
      <Text color={theme.dim} wrap="truncate-end">
        {'↑/↓ pgup/pgdn scroll · esc close'}
      </Text>
    </Box>
  );
}

/** A fixed-width column: pad short, TRUNCATE long.
 *
 * `padEnd` alone only pads — a value wider than its column overflows into the
 * next one, which is how `supervised` + `running` rendered as
 * `supervisedrunning`. A terminal table has no wrapping to save it, so the
 * column has to enforce its own width, and always leave one space so adjacent
 * values cannot touch even at exactly the limit. */
function col(v: unknown, width: number): string {
  const s = String(v ?? '');
  const w = width - 1;
  return (s.length > w ? `${s.slice(0, Math.max(1, w - 1))}…` : s).padEnd(width);
}

function statusColor(status: string): string {
  switch (status) {
    case 'running':
      return theme.accent;
    case 'completed':
      return theme.agent;
    case 'failed':
    case 'crashed':
    case 'killed':
      return theme.error;
    default:
      return theme.dim;
  }
}

export function SubagentList({
  s,
  selected,
}: {
  s: MirrorState;
  selected: number;
}): React.JSX.Element {
  const subs = [...s.subagents.values()] as { [k: string]: Json }[];
  if (subs.length === 0) {
    return <Text color={theme.dim}>no subagents yet — the agent spawns them as it delegates</Text>;
  }
  return (
    <Box flexDirection="column">
      <Text color={theme.dim} bold>
        {`  ${col('handle', 16)}${col('template', 12)}${col('reports to', 12)}${col('mode', 10)}${col('status', 10)}${col('tokens', 9)}updated`}
      </Text>
      {subs.slice(0, 20).map((x, i) => {
        const status = String(x.status ?? '');
        return (
          <Box key={String(x.handle ?? i)} flexDirection="row">
            <Text color={i === selected ? theme.accent : undefined} bold={i === selected}>
              {i === selected ? '▸ ' : '  '}
              {col(x.handle, 16)}
            </Text>
            <Text>{col(x.template ?? '', 12)}</Text>
            <Text color={theme.dim}>{col(x.reports_to ?? '', 12)}</Text>
            <Text color={theme.dim}>{col(x.mode, 10)}</Text>
            <Text color={statusColor(status)}>{col(status, 10)}</Text>
            <Text color={theme.dim}>{col(x.tokens ?? 0, 9)}</Text>
            <Text color={theme.dim}>{x.updated ? ago(Number(x.updated)) : ''}</Text>
          </Box>
        );
      })}
      <Text color={theme.dim}>{'\n↑/↓ select · enter details · tab next screen'}</Text>
    </Box>
  );
}

/** The detail view's message field and the exchange above it. */
export interface DetailCompose {
  /** The field is open and takes the keys. */
  open: boolean;
  value: string;
  cursor: number;
  onChange: (s: EditState) => void;
  onSubmit: (value: string) => void;
  /** What was sent and what came back, oldest first. */
  exchange: readonly ExchangeItem[];
  /** A reply is awaited. */
  waiting: boolean;
  /** The field's width in columns, inside its box. */
  width: number;
}

export function SubagentDetail({
  handle,
  summary,
  detail,
  debug,
  canSet = false,
  killAsk = false,
  compose,
  canLog = false,
}: {
  handle: string;
  summary: { [k: string]: Json } | undefined;
  detail: { [k: string]: Json } | null;
  /** The daemon offers the introspection reads (`introspectionOn`). */
  debug: boolean;
  /** The card lets this caller turn introspection on with `admin.set`. */
  canSet?: boolean;
  /** Waiting for confirmation that this subagent should be stopped. */
  killAsk?: boolean;
  /** The message field, when this client can send. */
  compose?: DetailCompose;
  /** The card lists `subagent.log` for this caller. */
  canLog?: boolean;
}): React.JSX.Element {
  const d = detail ?? summary ?? {};
  const line = (label: string, v: Json | undefined, color?: string) =>
    v === undefined || v === null ? null : (
      <Box key={label} flexDirection="row">
        <Text color={theme.dim}>{label.padEnd(14)}</Text>
        <Text color={color} wrap="wrap">
          {typeof v === 'string' ? v : JSON.stringify(v)}
        </Text>
      </Box>
    );
  return (
    <Box flexDirection="column">
      <Text color={theme.accent} bold>
        subagent {handle}
      </Text>
      {line('status', d.status, statusColor(String(d.status ?? '')))}
      {String(d.status ?? '') !== 'running' ? (
        <Text color={theme.dim}>{'(not running — messaging and stopping apply to a live subagent)'}</Text>
      ) : null}
      {line('template', d.template ?? summary?.template)}
      {line('reports to', d.reports_to ?? summary?.reports_to)}
      {line('mode', d.mode)}
      {line('attempt', d.attempt)}
      {line('tokens', d.tokens)}
      {line('instruction', d.instruction)}
      {line('result', d.result)}
      {line('error', d.error, theme.error)}
      {line('requested_by', d.requested_by)}
      {!debug ? (
        <Text color={theme.dim}>
          {`(summary only — instruction and result need ${DAEMON_KEYS.introspection}` +
            (canSet ? `: /set ${DAEMON_KEYS.introspection} true)` : ' in the daemon config)')}
        </Text>
      ) : null}
      {compose && compose.exchange.length > 0 ? (
        <Box flexDirection="column" marginTop={1} width={compose.width + 4}>
          {compose.exchange.slice(-6).map((x, i) => (
            <Text key={i} wrap="wrap" color={x.who === 'note' ? theme.dim : undefined}>
              <Text color={x.who === 'you' ? theme.accent : x.who === 'child' ? theme.agent : theme.dim} bold={x.who !== 'note'}>
                {x.who === 'you' ? 'you › ' : x.who === 'child' ? `${handle} › ` : '  '}
              </Text>
              {inertLines(x.text)}
            </Text>
          ))}
          {compose.waiting ? <Text color={theme.dim}>{`  waiting for ${handle}'s reply…`}</Text> : null}
        </Box>
      ) : null}
      {compose?.open ? (
        <Box flexDirection="column">
          {cannotMessage(summary) ? (
            <Text color={theme.warn} wrap="wrap">{`  ${cannotMessage(summary)}`}</Text>
          ) : null}
          <Box flexDirection="row" flexShrink={0} borderStyle="round" borderColor={theme.border} paddingX={1}>
            <Text color={theme.accent} bold>
              {'› '}
            </Text>
            {/* An explicit width, as the chat composer has: judged beside the
                prompt, a long message would wrap to more rows than its box. */}
            <Box width={Math.max(1, compose.width)} flexShrink={0}>
              <MultilineInput
                value={compose.value}
                cursor={compose.cursor}
                onChange={compose.onChange}
                onSubmit={compose.onSubmit}
                placeholder={`message ${handle}`}
              />
            </Box>
          </Box>
          <Text color={theme.dim}>{'enter send · esc leave the field'}</Text>
        </Box>
      ) : killAsk ? (
        <Text color={theme.error} bold>
          {'\nstop '}
          {handle}
          {'? y to confirm, any other key to cancel'}
        </Text>
      ) : (
        <Text color={theme.dim} wrap="wrap">
          {`\n${compose ? 'm/enter message · ' : ''}${canLog ? 'l log · ' : ''}k stop · esc/backspace back · tab next screen`}
        </Text>
      )}
    </Box>
  );
}
