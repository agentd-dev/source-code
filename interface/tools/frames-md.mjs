// SPDX-License-Identifier: AGPL-3.0-only
/**
 * Splice the captured frames into docs/interface.md, replacing whatever sits
 * inside each ```tui fence.
 *
 * The frames are generated from the shipped TUI, so this is what stops the
 * documentation describing a program that no longer exists: regenerate, and the
 * page tells the truth again.
 */
import { readFileSync, writeFileSync } from 'node:fs';

const frames = JSON.parse(readFileSync(new URL('../../docs/_generated/tui-frames.json', import.meta.url), 'utf8'));
const path = new URL('../../docs/interface.md', import.meta.url);
let md = readFileSync(path, 'utf8');

// Each fence carries `# <title>` as its first line; the title maps to a frame.
const TITLES = {
  'agentd tui — chat': 'chat',
  'agentd tui — subagents': 'subagents',
  'agentd tui — subagent detail': 'subagent-detail',
  'agentd tui — confirming a stop': 'subagent-stop',
  'agentd tui — debug': 'debug',
};

/** The colour codes chalk emits. The MARKDOWN gets the stripped text — a fence
 * full of escape bytes is unreadable on GitHub and churns every diff — while
 * the site reads the coloured original from `web/lib/tui-frames.json`, keyed by
 * the fence's own title so the two can never pair up wrongly. */
const stripAnsi = (s) => s.replace(/\u001b\[[0-9;]*m/g, '');

let replaced = 0;
md = md.replace(/```tui\n# ([^\n]+)\n[\s\S]*?```/g, (whole, title) => {
  const key = TITLES[title.trim()];
  if (!key || !frames[key]) return whole;
  replaced += 1;
  const plain = stripAnsi(frames[key]).replace(/\s+$/gm, '').trimEnd();
  return '```tui\n# ' + title + '\n' + plain + '\n```';
});
writeFileSync(path, md);

// The site's copy: coloured, keyed by title.
const byTitle = {};
for (const [title, key] of Object.entries(TITLES)) {
  if (frames[key]) byTitle[title] = frames[key].replace(/\s+$/gm, '').trimEnd();
}
writeFileSync(
  new URL('../../web/lib/tui-frames.json', import.meta.url),
  JSON.stringify(byTitle, null, 1),
);
console.error(`spliced ${replaced} frame(s) into docs/interface.md; wrote web/lib/tui-frames.json`);
