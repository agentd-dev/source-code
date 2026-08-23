// Generate public/og.png (1200×630) — the Open Graph / Twitter card image.
// Run manually when the branding changes: `node tools/og-image.mjs`.
// The PNG is committed; the build never needs sharp or fonts.
import sharp from "sharp";
import { writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const W = 1200;
const H = 630;

// The site's dark-terminal identity: near-black ground, a terminal window,
// the wordmark as a prompt. DejaVu Sans Mono renders everywhere sharp does.
const svg = `<svg width="${W}" height="${H}" viewBox="0 0 ${W} ${H}" xmlns="http://www.w3.org/2000/svg">
  <defs>
    <linearGradient id="bg" x1="0" y1="0" x2="1" y2="1">
      <stop offset="0" stop-color="#0b0f14"/>
      <stop offset="1" stop-color="#10161f"/>
    </linearGradient>
    <linearGradient id="accent" x1="0" y1="0" x2="1" y2="0">
      <stop offset="0" stop-color="#34d399"/>
      <stop offset="1" stop-color="#22d3ee"/>
    </linearGradient>
  </defs>
  <rect width="${W}" height="${H}" fill="url(#bg)"/>

  <!-- terminal window -->
  <rect x="90" y="96" width="1020" height="438" rx="14" fill="#0d1420" stroke="#1f2a3a" stroke-width="2"/>
  <rect x="90" y="96" width="1020" height="46" rx="14" fill="#131c2b"/>
  <rect x="90" y="120" width="1020" height="22" fill="#131c2b"/>
  <circle cx="124" cy="119" r="7" fill="#ef4444"/>
  <circle cx="148" cy="119" r="7" fill="#f59e0b"/>
  <circle cx="172" cy="119" r="7" fill="#22c55e"/>
  <text x="600" y="126" font-family="DejaVu Sans Mono" font-size="17" fill="#5b6b82" text-anchor="middle">agentd — daemon</text>

  <!-- prompt + wordmark -->
  <text x="150" y="252" font-family="DejaVu Sans Mono" font-size="34" fill="#34d399">$</text>
  <text x="188" y="260" font-family="DejaVu Sans Mono" font-size="96" font-weight="bold" fill="#e6edf3">agentd</text>
  <rect x="560" y="188" width="34" height="86" fill="url(#accent)" opacity="0.9"/>

  <text x="150" y="330" font-family="DejaVu Sans Mono" font-size="31" fill="#9fb0c3">the runtime for autonomous AI agents</text>

  <!-- capability line -->
  <text x="150" y="404" font-family="DejaVu Sans Mono" font-size="22" fill="#5b6b82">one static binary · durable workflows · event streams</text>
  <text x="150" y="440" font-family="DejaVu Sans Mono" font-size="22" fill="#5b6b82">tools over MCP · speaks A2A · runs no code of its own</text>

  <!-- footer -->
  <text x="150" y="500" font-family="DejaVu Sans Mono" font-size="24" fill="#34d399">agentd.dev</text>
  <text x="1050" y="500" font-family="DejaVu Sans Mono" font-size="22" fill="#5b6b82" text-anchor="end">Apache-2.0 · Rust</text>
</svg>`;

const here = dirname(fileURLToPath(import.meta.url));
const out = join(here, "..", "public", "og.png");
const png = await sharp(Buffer.from(svg)).png().toBuffer();
writeFileSync(out, png);
console.log(`wrote ${out} (${png.length} bytes)`);
