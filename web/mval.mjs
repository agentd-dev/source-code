import fs from 'fs';
const mod = await import('mermaid/dist/chunks/mermaid.core/flowDiagram-UKHOOZJN.mjs');
function check(src) {
  const d = mod.createFlowDiagram();
  const p = d.parser;
  if (p.parser) p.parser.yy = d.db; else p.yy = d.db;
  if (d.db.clear) d.db.clear();
  p.parse(src);
  const v = d.db.getVertices?.();
  const e = d.db.getEdges?.();
  return `nodes=${v ? (v.size ?? Object.keys(v).length) : '?'} edges=${e ? e.length : '?'}`;
}
function run(file, label) {
  const t = fs.readFileSync(file, 'utf8');
  const blocks = [...t.matchAll(/```mermaid\n([\s\S]*?)```/g)].map(x => x[1]);
  blocks.forEach((b, i) => {
    if (!/^\s*(flowchart|graph)\b/m.test(b)) { console.log(label, i, 'skip'); return; }
    try { console.log(label, i, 'OK', check(b)); }
    catch (err) { console.log(label, i, 'FAIL', String(err.message).slice(0, 300)); }
  });
}
run('/root/agentd-dev/source-code/docs/architecture.md', 'CONTROL');
run('/root/agentd-dev/source-code/docs/security.md', 'SECURITY');
