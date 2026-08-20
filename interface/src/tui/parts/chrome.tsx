// SPDX-License-Identifier: Apache-2.0
/**
 * The daemon-driven chrome (RFC 0032 §12): the top (header) and bottom
 * (status bar) edges render exactly the item list `interface.display`
 * declares — the DAEMON decides what its operators see; every attached client
 * lays out the same. Unknown items are skipped (forward compatibility).
 */
import React from 'react';
import { Box, Text } from 'ink';
import type { MirrorState } from '../../client/index.js';
import { theme } from '../theme.js';

export interface ChromeCtx {
  s: MirrorState;
  endpoint: string;
  screen: string;
  active: number;
}

function counters(s: MirrorState):
  | { turns?: number; tokens_in?: number; tokens_out?: number; tool_calls?: number }
  | undefined {
  return ((s.status ?? s.bootstrap) as { counters?: { turns?: number; tokens_in?: number; tokens_out?: number; tool_calls?: number } } | undefined)
    ?.counters;
}

/** Render one display item (null ⇒ skip). */
function item(name: string, c: ChromeCtx): React.JSX.Element | null {
  const { s } = c;
  const key = name;
  // `memory:<key>` is whatever a WORKFLOW put there — a branch, a PR number, a
  // deploy state. The client does not know what it means and does not need to:
  // it renders the value the daemon resolved. An unset key renders nothing at
  // all rather than an empty slot, because a blank status reads as broken.
  if (name.startsWith('memory:')) {
    const v = s.info?.display?.values?.[name];
    if (v === undefined || v === null || v === '') return null;
    return (
      <Text key={key} color={theme.command}>
        {typeof v === 'string' ? v : JSON.stringify(v)}
      </Text>
    );
  }
  switch (name) {
    case 'name':
      return (
        <Text key={key} color={theme.accent} bold>
          {((s.card as { name?: string } | undefined)?.name ?? 'agentd') as string}
        </Text>
      );
    case 'version':
      return s.info ? <Text key={key} color={theme.dim}>{s.info.version}</Text> : null;
    case 'instance':
      return s.info ? <Text key={key} color={theme.accent}>{s.info.instance}</Text> : null;
    case 'model':
      return s.info?.model ? <Text key={key} color={theme.dim}>{s.info.model}</Text> : null;
    case 'endpoint':
      return <Text key={key} color={theme.dim}>{c.endpoint}</Text>;
    case 'conn': {
      const map: Record<string, [string, string]> = {
        ready: ['● live', theme.accent],
        polling: ['◐ polling', theme.warn],
        connecting: ['○ connecting', theme.dim],
        error: [`✗ ${s.error ?? 'error'}`, theme.error],
        closed: ['○ closed', theme.dim],
      };
      const [label, color] = map[s.conn] ?? ['?', theme.dim];
      return (
        <Text key={key} color={color}>
          {label}
        </Text>
      );
    }
    case 'debug':
      return s.info?.debug ? (
        <Text key={key} color={theme.warn}>
          debug
        </Text>
      ) : null;
    case 'draining':
      return s.draining ? (
        <Text key={key} color={theme.warn} bold>
          DRAINING
        </Text>
      ) : s.paused ? (
        <Text key={key} color={theme.warn} bold>
          PAUSED
        </Text>
      ) : null;
    case 'active':
      return c.active > 0 ? (
        <Text key={key} color={theme.accent}>
          {c.active} active
        </Text>
      ) : null;
    case 'turns': {
      const n = counters(s)?.turns;
      return n !== undefined ? <Text key={key} color={theme.dim}>{n} turns</Text> : null;
    }
    case 'tokens': {
      const ct = counters(s);
      return ct ? (
        <Text key={key} color={theme.dim}>
          {ct.tokens_in ?? 0}/{ct.tokens_out ?? 0} tok
        </Text>
      ) : null;
    }
    case 'tool_calls': {
      const n = counters(s)?.tool_calls;
      return n !== undefined ? <Text key={key} color={theme.dim}>{n} tools</Text> : null;
    }
    case 'runs':
      return <Text key={key} color={theme.dim}>{s.runs.size} runs</Text>;
    case 'subagents':
      return <Text key={key} color={theme.dim}>{s.subagents.size} subagents</Text>;
    case 'conversations':
      return <Text key={key} color={theme.dim}>{s.conversations.size} conv</Text>;
    case 'screen':
      return <Text key={key} color={theme.dim}>[{c.screen}]</Text>;
    case 'keys':
      return (
        <Text key={key} color={theme.dim}>
          tab:screens esc:cancel /:cmd ^c:quit
        </Text>
      );
    case 'clock':
      return (
        <Text key={key} color={theme.dim}>
          {new Date().toLocaleTimeString()}
        </Text>
      );
    default:
      return null; // unknown item — skip (forward compatibility)
  }
}

export function Edge({ items, ctx }: { items: string[]; ctx: ChromeCtx }): React.JSX.Element {
  const rendered = items.map((n) => item(n, ctx)).filter((x): x is React.JSX.Element => x !== null);
  // Explicit per-item margins (not `gap`): ink collapses gap between bare Text
  // children in some layouts, gluing adjacent items.
  return (
    <Box flexDirection="row" flexWrap="wrap">
      {rendered.map((el) => (
        <Box key={el.key ?? undefined} marginRight={1}>
          {el}
        </Box>
      ))}
    </Box>
  );
}

export const DEFAULT_TOP = ['name', 'version', 'instance', 'debug'];
export const DEFAULT_BOTTOM = [
  'conn',
  'endpoint',
  'draining',
  'active',
  'turns',
  'tokens',
  'screen',
  'keys',
];
