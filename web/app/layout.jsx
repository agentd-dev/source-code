import "./globals.css";
import Link from "next/link";

export const metadata = {
  title: "agentd — the bounded agent runtime",
  description:
    "A predeclared DAG walks. An LLM fills one node. Nothing improvises. " +
    "Single-binary workflow runtime with compile-time capability pruning.",
};

export default function RootLayout({ children }) {
  return (
    <html lang="en">
      <body className="crt min-h-screen">
        <header className="frame border-x-0 border-t-0 sticky top-0 z-40 backdrop-blur bg-[var(--bg)]/90">
          <nav className="mx-auto flex max-w-5xl items-center gap-6 px-4 py-3 text-sm">
            <Link href="/" className="text-[var(--accent)] font-bold">
              agentd<span className="text-[var(--dim)]">@~</span>
              <span className="cursor" />
            </Link>
            <Link href="/docs/overview/" className="text-[var(--dim)] hover:text-[var(--accent)]">
              docs
            </Link>
            <Link href="/docs/rfc-0001/" className="text-[var(--dim)] hover:text-[var(--accent)]">
              rfcs
            </Link>
            <a
              href="https://github.com/agentd-dev/source-code"
              className="ml-auto text-[var(--dim)] hover:text-[var(--accent)]"
            >
              [github]
            </a>
          </nav>
        </header>
        {children}
        <footer className="mx-auto max-w-5xl px-4 py-10 text-xs text-[var(--dim)]">
          <span className="text-[var(--accent)]">$</span> echo "MIT licensed · built with a
          predeclared DAG, like everything else here"
        </footer>
      </body>
    </html>
  );
}
