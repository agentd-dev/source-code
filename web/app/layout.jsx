import "./globals.css";
import Link from "next/link";
import Script from "next/script";
import Nav from "./components/Nav";

export const metadata = {
  metadataBase: new URL("https://agentd.dev"),
  title: "agentd — a small, MCP-native agent runtime",
  description:
    "agentd is a minimal agent runtime: one static binary that takes an instruction " +
    "and tools from remote MCP servers over HTTPS and runs the agentic loop — as a " +
    "one-shot job, a long-lived daemon, or a durable workflow. It runs no code of " +
    "its own, speaks A2A to other agents, and is supervised, bounded and observable.",
  keywords: [
    "agentd",
    "MCP",
    "Model Context Protocol",
    "A2A",
    "Agent2Agent",
    "AI agent",
    "agent runtime",
    "agentic workflow",
    "cloud native",
    "kubernetes",
    "Rust",
  ],
  openGraph: {
    title: "agentd — a small, MCP-native agent runtime",
    description:
      "An instruction, tools from remote MCP servers over HTTPS, one static binary. Run the agentic loop as a one-shot, a daemon, or a durable workflow. Speaks A2A. Runs no code of its own.",
    type: "website",
    url: "https://agentd.dev",
  },
};

/* Apply the stored theme before first paint — otherwise a dark-mode reader
   gets a white flash on every navigation. Kept tiny and inline on purpose. */
const NO_FLASH = `(function(){try{var t=localStorage.getItem('theme');
if(t==='light'||t==='dark'){document.documentElement.setAttribute('data-theme',t);}
}catch(e){}})();`;

function Footer() {
  return (
    <footer className="mt-24 border-t border-[var(--line)]">
      <div className="mx-auto max-w-[82rem] px-4 py-12">
        <div className="grid gap-8 sm:grid-cols-2 lg:grid-cols-4">
          <div>
            <div className="brand text-base">
              agentd<span className="at">@</span>
              <span className="tilde">~</span>
            </div>
            <p className="mt-2 max-w-xs text-sm text-[var(--dim)]">
              A minimal, MCP-native agent runtime. One static binary, supervised and bounded.
            </p>
          </div>
          <FooterCol
            title="Learn"
            links={[
              ["Overview", "/docs/overview/"],
              ["Getting started", "/docs/getting-started/"],
              ["Coding agent", "/docs/coding-agent/"],
              ["Use cases", "/docs/use-cases/"],
            ]}
          />
          <FooterCol
            title="Concepts"
            links={[
              ["Architecture", "/docs/architecture/"],
              ["The harness", "/docs/harness/"],
              ["Workflows", "/docs/workflows/"],
              ["Why Rust", "/docs/why-rust/"],
            ]}
          />
          <FooterCol
            title="Reference"
            links={[
              ["Configuration", "/docs/configuration/"],
              ["TUI & web UI", "/docs/interface/"],
              ["Security", "/docs/security/"],
              ["All docs", "/docs/"],
            ]}
          />
        </div>
        <div className="mt-10 flex flex-col gap-3 border-t border-[var(--line)] pt-6 text-xs text-[var(--dim)] sm:flex-row sm:items-center sm:justify-between">
          <div>Apache-2.0 · built in the open</div>
          <div className="flex gap-5">
            {/* The LLM-facing reference. Served from `web/public/` so it sits at
                the site root, which is where the llms.txt convention expects a
                crawler or an agent to look for it. */}
            <a href="/llms.txt" className="hover:text-[var(--fg-strong)]">
              llms.txt
            </a>
            <a href="https://github.com/agentd-dev/source-code" className="hover:text-[var(--fg-strong)]">
              GitHub
            </a>
            <a href="https://modelcontextprotocol.io" className="hover:text-[var(--fg-strong)]">
              MCP ↗
            </a>
            <a href="https://a2a-protocol.org" className="hover:text-[var(--fg-strong)]">
              A2A ↗
            </a>
          </div>
        </div>
      </div>
    </footer>
  );
}

function FooterCol({ title, links }) {
  return (
    <div>
      <div className="mb-3 font-mono text-[0.68rem] uppercase tracking-[0.1em] text-[var(--dim)]">
        {title}
      </div>
      <ul className="space-y-1.5 text-sm">
        {links.map(([label, href]) => (
          <li key={href}>
            <Link href={href} className="text-[var(--fg)] hover:text-[var(--green)]">
              {label}
            </Link>
          </li>
        ))}
      </ul>
    </div>
  );
}

export default function RootLayout({ children }) {
  return (
    <html lang="en" suppressHydrationWarning>
      <head>
        <script dangerouslySetInnerHTML={{ __html: NO_FLASH }} />
      </head>
      <body className="min-h-screen">
        <Script
          defer
          strategy="afterInteractive"
          data-domain="agentd.dev"
          src="https://analytics.tsok.org/js/script.js"
        />
        <a href="#content" className="skip">
          Skip to content
        </a>
        <Nav />
        <div id="content" tabIndex={-1}>
          {children}
        </div>
        <Footer />
      </body>
    </html>
  );
}
