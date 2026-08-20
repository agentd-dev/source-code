// SPDX-License-Identifier: Apache-2.0
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

let replaced = 0;
md = md.replace(/```tui\n# ([^\n]+)\n[\s\S]*?```/g, (whole, title) => {
  const key = TITLES[title.trim()];
  if (!key || !frames[key]) return whole;
  replaced += 1;
  return '```tui\n# ' + title + '\n' + frames[key].replace(/\s+$/gm, '').trimEnd() + '\n```';
});
writeFileSync(path, md);
console.error(`spliced ${replaced} frame(s) into docs/interface.md`);
