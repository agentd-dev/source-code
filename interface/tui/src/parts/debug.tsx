// SPDX-License-Identifier: AGPL-3.0-only
/**
 * The debug screen ("extra information") — rendered only when the DAEMON says
 * so (`interface.debug`, learned from `interface.info`): the live feed tail,
 * runs with step progress, the subagent/child tables, and the log-ring tail.
 */
import React from 'react';
import { Box, Text } from 'ink';
import type { FeedEvent, Json, MirrorState } from '@agentd/client';
import { ago, shortId, theme } from '../theme.js';

function one(v: Json): string {
  const s = typeof v === 'string' ? v : JSON.stringify(v);
  return s.length > 100 ? `${s.slice(0, 100)}…` : s;
}

function FeedTail({ events }: { events: FeedEvent[] }): React.JSX.Element {
  const tail = events.slice(-8);
  return (
    <Box flexDirection="column">
      <Text bold color={theme.accent}>
        feed
      </Text>
      {tail.length === 0 ? <Text color={theme.dim}>quiet</Text> : null}
      {tail.map((e) => (
        <Text key={e.seq} color={theme.dim} wrap="truncate-end">
          {String(e.seq).padStart(5)} <Text color={theme.command}>{e.kind.padEnd(14)}</Text>
          {one(e.data)}
        </Text>
      ))}
    </Box>
  );
}

function Runs({ s }: { s: MirrorState }): React.JSX.Element {
  const runs = [...s.runs.values()].slice(-6) as { [k: string]: Json }[];
  return (
    <Box flexDirection="column">
      <Text bold color={theme.accent}>
        runs
      </Text>
      {runs.length === 0 ? <Text color={theme.dim}>none</Text> : null}
      {runs.map((r, i) => (
        <Text key={(r.id as string) ?? i} wrap="truncate-end">
          <Text color={theme.command}>{shortId((r.id as string) ?? '?', 22).padEnd(23)}</Text>
          <Text>{String(r.status ?? '').padEnd(11)}</Text>
          <Text color={theme.dim}>{r.steps ? one(r.steps) : ''}</Text>
        </Text>
      ))}
    </Box>
  );
}

function Procs({ s }: { s: MirrorState }): React.JSX.Element {
  const subs = [...s.subagents.values()] as { [k: string]: Json }[];
  const kids = [...s.children.values()] as { [k: string]: Json }[];
  return (
    <Box flexDirection="column">
      <Text bold color={theme.accent}>
        subagents / children
      </Text>
      {subs.length + kids.length === 0 ? <Text color={theme.dim}>none</Text> : null}
      {subs.slice(-4).map((x, i) => (
        <Text key={`s${i}`} color={theme.dim} wrap="truncate-end">
          sub {String(x.handle ?? '').padEnd(18)} {String(x.status ?? '').padEnd(10)} {String(x.tokens ?? 0)} tok
        </Text>
      ))}
      {kids.slice(-4).map((x, i) => (
        <Text key={`c${i}`} color={theme.dim} wrap="truncate-end">
          pid {String(x.pid ?? '').padEnd(8)} {String(x.kind ?? '').padEnd(18)} {ago(Date.now() - Number(x.age_ms ?? 0))}
        </Text>
      ))}
    </Box>
  );
}

function LogTail({ lines }: { lines: Json[] }): React.JSX.Element {
  const tail = lines.slice(-8) as { [k: string]: Json }[];
  return (
    <Box flexDirection="column">
      <Text bold color={theme.accent}>
        log (debug.events)
      </Text>
      {tail.length === 0 ? <Text color={theme.dim}>—</Text> : null}
      {tail.map((l, i) => (
        <Text
          key={(l.seq as number) ?? i}
          color={l.level === 'warn' || l.level === 'error' ? theme.warn : theme.dim}
          wrap="truncate-end"
        >
          {String(l.level ?? '').padEnd(6)}
          <Text color={theme.command}>{String(l.event ?? '').padEnd(24)}</Text>
          {one(l)}
        </Text>
      ))}
    </Box>
  );
}

export function DebugScreen({
  s,
  logLines,
}: {
  s: MirrorState;
  logLines: Json[];
}): React.JSX.Element {
  if (!s.info?.debug) {
    return (
      <Text color={theme.dim}>
        debug is off on this daemon — set interface.debug: true (or run agentd tui --debug)
      </Text>
    );
  }
  return (
    <Box flexDirection="column" gap={1}>
      <FeedTail events={s.feedLog} />
      <Runs s={s} />
      <Procs s={s} />
      <LogTail lines={logLines} />
    </Box>
  );
}
