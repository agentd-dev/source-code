// SPDX-License-Identifier: AGPL-3.0-only
// A scripted model for the rehearsals of demos (c) and (e): it speaks enough
// of the OpenAI chat-completions API for agentd to drive a turn, and answers
// from a playbook whose answers QUOTE the request — the instruction agentd
// delivered in the system prompt — instead of carrying fixed text. So when
// the instruction changes, the answer changes, and only because it did.
//
//   node model.mjs --playbook ../mock/support.model.json [--port 8729]
//   agentd … --intelligence http://127.0.0.1:8729/v1/chat/completions \
//            --intelligence-token "" --model scripted
//
// It is not intelligence: it matches and copies. agentd's built-in mock
// (`mock:file:…`) answers with fixed text, which is right for demos (a), (b)
// and (d); this exists because demo (c) must show the deadline the customer
// hears come FROM the disclosure line, and demo (e) the temperature from the
// rule.
//
// The playbook: the first rule whose `when` holds and whose every `capture`
// matches answers, with `{{name}}` in `content` and in tool-call arguments
// replaced by what the capture's first group matched. An argument that is
// exactly "{{name:number}}" becomes a JSON number.
//
//   { "rules": [ {
//       "name": "disclosure",
//       "when": { "last_user": "dispute", "no_tool_result": true },
//       "capture": { "deadline": { "from": "system", "re": "say \"(You have [^\"]+)\"" } },
//       "content": "… {{deadline}} …"
//   } ] }
//
// `when` keys, all regular expressions (case-insensitive) but the last:
//   system, last_user, last_tool    the system prompt, the newest user message, and the
//                                   newest tool result since it ('' before any)
//   no_tool_result                  true: only before any tool result in this turn's transcript
//                                   (counted after the newest user message)
// `capture[].from`: system (default), last_user or last_tool.
// `tool_calls`: [{name, arguments}] — `name` is the tool as agentd names it
// (`thermostat.target.set`); the stand-in finds it in the request's tools.
import { createServer } from 'node:http';
import { readFileSync } from 'node:fs';
import { arg, logger } from './lib.mjs';

const port = Number(arg('port', process.env.MODEL_PORT || 8729));
const file = arg('playbook');
if (!file) {
  console.error('model.mjs: --playbook <file.json> is required');
  process.exit(2);
}
const log = logger('model');
const playbook = () => JSON.parse(readFileSync(file, 'utf8')); // re-read: edit it live if you like

const text = (m) => (typeof m?.content === 'string' ? m.content
  : Array.isArray(m?.content) ? m.content.map((p) => p.text ?? '').join('') : '');

function views(req) {
  const msgs = req.messages ?? [];
  const lastUser = msgs.findLastIndex((m) => m.role === 'user');
  const sinceUser = msgs.slice(lastUser + 1);
  return {
    system: msgs.filter((m) => m.role === 'system').map(text).join('\n'),
    last_user: lastUser >= 0 ? text(msgs[lastUser]) : '',
    last_tool: text(sinceUser.findLast((m) => m.role === 'tool')),
    tool_results: sinceUser.filter((m) => m.role === 'tool').length,
  };
}

function fill(value, vars) {
  if (typeof value === 'string') {
    const whole = value.match(/^\{\{(\w+):number\}\}$/);
    if (whole) return Number(vars[whole[1]]);
    return value.replace(/\{\{(\w+)\}\}/g, (_, k) => vars[k] ?? '');
  }
  if (Array.isArray(value)) return value.map((v) => fill(v, vars));
  if (value && typeof value === 'object') return Object.fromEntries(Object.entries(value).map(([k, v]) => [k, fill(v, vars)]));
  return value;
}

/** The tool as the request names it: agentd's name, or its wire-safe spelling. */
function toolName(req, want) {
  const names = (req.tools ?? []).map((t) => t.function?.name ?? t.name).filter(Boolean);
  const safe = (s) => s.replace(/[^A-Za-z0-9_-]/g, '_');
  return names.find((n) => n === want) ?? names.find((n) => safe(n) === safe(want)) ?? want;
}

function answer(req) {
  const v = views(req);
  for (const rule of playbook().rules ?? []) {
    const w = rule.when ?? {};
    if (w.no_tool_result && v.tool_results > 0) continue;
    if (['system', 'last_user', 'last_tool'].some((k) => w[k] && !new RegExp(w[k], 'i').test(v[k]))) continue;
    const vars = {};
    let missing = null;
    for (const [name, c] of Object.entries(rule.capture ?? {})) {
      const m = (v[c.from ?? 'system'] ?? '').match(new RegExp(c.re));
      if (!m) { missing = name; break; }
      vars[name] = m[1];
    }
    if (missing) {
      log('capture.missed', { rule: rule.name, capture: missing });
      continue;
    }
    if (rule.tool_calls) {
      const calls = rule.tool_calls.map((c, i) => ({
        id: `call_${Date.now().toString(36)}_${i + 1}`, type: 'function',
        function: { name: toolName(req, c.name), arguments: JSON.stringify(fill(c.arguments ?? {}, vars)) },
      }));
      log('answered', { rule: rule.name, tool_calls: calls.map((c) => `${c.function.name} ${c.function.arguments}`) });
      return { role: 'assistant', content: null, tool_calls: calls };
    }
    const content = fill(rule.content ?? '', vars);
    log('answered', { rule: rule.name, ...vars, text: content });
    return { role: 'assistant', content };
  }
  log('answered', { rule: '(none)' });
  return { role: 'assistant', content: 'The scripted model has no rule for this request.' };
}

const http = createServer(async (req, res) => {
  const host = (req.headers.host ?? '').replace(/:\d+$/, '');
  if (!['127.0.0.1', 'localhost', '[::1]'].includes(host)) return void res.writeHead(403).end();
  if (req.method !== 'POST' || !req.url.endsWith('/chat/completions')) return void res.writeHead(404).end();
  const chunks = [];
  for await (const c of req) chunks.push(c);
  let out;
  try {
    const message = answer(JSON.parse(Buffer.concat(chunks).toString('utf8')));
    out = {
      id: `chatcmpl-${Date.now().toString(36)}`, object: 'chat.completion', model: 'scripted',
      choices: [{ index: 0, message, finish_reason: message.tool_calls ? 'tool_calls' : 'stop' }],
      usage: { prompt_tokens: 40, completion_tokens: 20, total_tokens: 60 },
    };
  } catch (e) {
    log('error', { message: String(e?.message ?? e) });
    return void res.writeHead(400, { 'content-type': 'application/json' }).end(JSON.stringify({ error: { message: String(e) } }));
  }
  res.writeHead(200, { 'content-type': 'application/json' }).end(JSON.stringify(out));
});
http.listen(port, '127.0.0.1', () => log('listening', { url: `http://127.0.0.1:${port}/v1/chat/completions`, playbook: file }));
for (const sig of ['SIGINT', 'SIGTERM']) process.once(sig, () => { http.closeAllConnections?.(); http.close(() => process.exit(0)); });
