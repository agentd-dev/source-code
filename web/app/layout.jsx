import "./globals.css";
import Link from "next/link";

export const metadata = {
  metadataBase: new URL("https://agentd.dev"),
  title: "agentd — a small, cloud-native AI agent daemon, MCP-native",
  description:
    "agentd is a minimal, MCP-native, reactive agent runtime: one static binary " +
    "that takes an instruction and tools from MCP servers and runs the agentic " +
    "loop — as a one-shot, a daemon, or a reactive service. Supervised, bounded, " +
    "observable. ~1.3 MB, 3 dependencies, k8s-ready.",
  keywords: [
    "agentd",
    "MCP",
    "Model Context Protocol",
    "AI agent",
    "agent runtime",
    "cloud native",
    "kubernetes",
    "daemon",
    "Rust",
  ],
  openGraph: {
    title: "agentd — a small, cloud-native AI agent daemon",
    description:
      "An instruction + tools from MCP + one static binary. Run the agentic loop as a one-shot, a daemon, or a reactive service.",
    type: "website",
    url: "https://agentd.dev",
  },
};

function Nav() {
  return (
    <header className="sticky top-0 z-40 border-b border-[var(--line)] bg-[var(--bg)]/80 backdrop-blur">
      <nav aria-label="primary" className="mx-auto flex max-w-5xl items-center gap-6 px-4 py-3 text-sm">
        <Link href="/" className="font-bold text-[var(--fg-strong)]">
          agentd<span className="text-[var(--dim)]">@</span>
          <span className="text-[var(--green)]">~</span>
        </Link>
        <Link href="/#mcp" className="hidden text-[var(--dim)] hover:text-[var(--fg-strong)] sm:inline">
          mcp
        </Link>
        <Link href="/#capabilities" className="hidden text-[var(--dim)] hover:text-[var(--fg-strong)] sm:inline">
          capabilities
        </Link>
        <Link href="/#run" className="hidden text-[var(--dim)] hover:text-[var(--fg-strong)] sm:inline">
          run it
        </Link>
        <Link href="/docs/overview/" className="text-[var(--dim)] hover:text-[var(--fg-strong)]">
          docs
        </Link>
        <a
          href="https://github.com/agentd-dev/source-code"
          className="ml-auto text-[var(--dim)] hover:text-[var(--green)]"
        >
          github ↗
        </a>
      </nav>
    </header>
  );
}

function Footer() {
  return (
    <footer className="mt-24 border-t border-[var(--line)]">
      <div className="mx-auto flex max-w-5xl flex-col gap-4 px-4 py-10 text-xs text-[var(--dim)] sm:flex-row sm:items-center sm:justify-between">
        <div>
          <span className="text-[var(--green)]">$</span>{" "}
          <span className="text-[var(--fg)]">agentd</span> — a minimal, MCP-native, reactive agent
          runtime · MIT
        </div>
        <div className="flex gap-5">
          <Link href="/docs/overview/" className="hover:text-[var(--fg-strong)]">
            docs
          </Link>
          <Link href="/docs/rfc-0001/" className="hover:text-[var(--fg-strong)]">
            rfcs
          </Link>
          <a
            href="https://github.com/agentd-dev/source-code"
            className="hover:text-[var(--green)]"
          >
            github
          </a>
          <a
            href="https://modelcontextprotocol.io"
            className="hover:text-[var(--fg-strong)]"
          >
            mcp ↗
          </a>
        </div>
      </div>
    </footer>
  );
}

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body className="min-h-screen">
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
