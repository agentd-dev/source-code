import "./globals.css";
import Link from "next/link";
import ResourcesMenu from "./resources-menu";

export const metadata = {
  title: "agentd — spin up an AI agent that works on its own",
  description:
    "Spin up an AI agent that works on its own. Give it a task, a goal, or a " +
    "whole workflow — agentd runs as a daemon (or a one-shot), calls tools, " +
    "and self-corrects, with every step governed, observable, and audited.",
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
            {/* Mobile: rfcs / use cases / inspect group under one
                dropdown; docs and [github] stay directly tappable. */}
            <ResourcesMenu />
            <Link
              href="/use-cases/"
              className="hidden md:inline text-[var(--dim)] hover:text-[var(--accent)]"
            >
              use cases
            </Link>
            <Link href="/docs/overview/" className="text-[var(--dim)] hover:text-[var(--accent)]">
              docs
            </Link>
            <Link
              href="/docs/rfc-0001/"
              className="hidden md:inline text-[var(--dim)] hover:text-[var(--accent)]"
            >
              rfcs
            </Link>
            <Link
              href="/inspect/"
              className="hidden md:inline text-[var(--dim)] hover:text-[var(--accent)]"
            >
              inspect
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
