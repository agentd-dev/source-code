import Link from "next/link";

const BANNER = String.raw`                            _      _
  __ _  __ _  ___ _ __  __| |  __| |
 / _' |/ _' |/ _ \ '_ \/ _' | / _' |
| (_| | (_| |  __/ | | | (_| || (_| |
 \__,_|\__, |\___|_| |_|\__,_| \__,_|
       |___/   the bounded agent runtime`;

const LOOP = String.raw`your workflow.toml ──► validate (build-time + load-time)
                              │
   trigger ───────────────────▼────────────────────────────┐
   (HTTP / cron / fs-watch /  │  ENGINE: walk the DAG      │
    manual --input)           │  one node at a time        │
        ┌─────────────────────┼─────────────────────┐      │
        │ read_file · parse_json · template_render  │      │
        │ llm_infer ◄── bounded reasoning step      │      │
        │ write_file · http_request · call_mcp_tool │      │
        │ switch / condition / fail / terminate     │      │
        └─────────────────────┬─────────────────────┘      │
                              │ policy + budgets + deadline│
                              ▼                            │
                    outcome JSON + execution trace ◄───────┘`;

const FEATURES = [
  {
    k: "bounded by construction",
    v: "The LLM is one node type with a prompt template and a JSON contract. It cannot add nodes, pick edges, or invent tool calls — routing on its output is a switch node you declared.",
  },
  {
    k: "capabilities are compile-time",
    v: "Tool families are Cargo features. A build without tools-http cannot make an outbound request — the code is not in the binary. CI proves every canonical feature set.",
  },
  {
    k: "fail-closed policy",
    v: "Allowlists per family: fs paths, env keys, URLs, shell commands, MCP tools. Empty sections deny. Optional Rego layers on as a logical AND.",
  },
  {
    k: "triggers, not prompts",
    v: "HTTP webhooks (bearer / HMAC / mTLS / OIDC, rate-limited), cron + interval schedules, debounced filesystem watches, or one-shot CLI runs.",
  },
  {
    k: "signed + traced",
    v: "ed25519 signatures verified over raw TOML bytes before anything parses trust. Every run yields the exact node path it walked; audit events stream to a redacting JSONL sink.",
  },
  {
    k: "one small binary",
    v: "Hand-rolled HTTP/1.1 both directions, no async runtime in the core, ~25 MB distroless image, systemd-hardened unit, deb/rpm packages.",
  },
];

export default function Home() {
  return (
    <main className="mx-auto max-w-5xl px-4">
      <section className="py-14">
        <pre className="text-[var(--green)] text-[10px] leading-tight sm:text-xs overflow-x-auto">
          {BANNER}
        </pre>
        <p className="mt-6 max-w-2xl text-lg">
          A predeclared DAG walks. An LLM fills one node.{" "}
          <span className="text-[var(--green)]">Nothing improvises.</span>
        </p>
        <div className="frame mt-8">
          <div className="frame-title">
            <span className="dot" />
            <span className="dot" />
            <span className="dot" />
            <span>agentd — serve</span>
          </div>
          <div className="p-4 text-sm leading-7 overflow-x-auto">
            <div>
              <span className="text-[var(--dim)]">$ </span>
              <span className="typed inline-block align-bottom">
                agentd --config webhook.toml --bind 127.0.0.1:8080
              </span>
            </div>
            <div className="text-[var(--dim)]">
              agentd: workflow `webhook_receiver` listening on http://127.0.0.1:8080/ (1 routes;
              drain_timeout=30s)
            </div>
            <div className="text-[var(--dim)]">
              {"       "}POST /hooks/github → on_hook
            </div>
            <div className="text-[var(--dim)]">{"       "}GET /healthz is always live</div>
            <div>
              <span className="text-[var(--amber)]">audit</span>{" "}
              <span className="text-[var(--dim)]">
                event=workflow.completed last_node=done elapsed_ms=3
              </span>
              <span className="cursor" />
            </div>
          </div>
        </div>
        <div className="mt-8 flex flex-wrap gap-4 text-sm">
          <Link
            href="/docs/overview/"
            className="frame px-4 py-2 text-[var(--green)] hover:bg-[var(--line)]"
          >
            $ man agentd
          </Link>
          <a
            href="https://github.com/agentd-dev/source-code"
            className="frame px-4 py-2 text-[var(--dim)] hover:text-[var(--green)] hover:bg-[var(--line)]"
          >
            $ git clone agentd-dev/source-code
          </a>
        </div>
      </section>

      <section className="py-10">
        <h2 className="text-[var(--green)] text-sm tracking-widest">
          <span className="text-[var(--dim)]">##</span> THE LOOP — SINGULAR, AND YOU WROTE IT
        </h2>
        <div className="frame mt-4">
          <div className="frame-title">
            <span className="dot" /> execution model
          </div>
          <pre className="p-4 text-xs sm:text-sm leading-relaxed overflow-x-auto text-[var(--fg)]">
            {LOOP}
          </pre>
        </div>
      </section>

      <section className="py-10">
        <h2 className="text-[var(--green)] text-sm tracking-widest">
          <span className="text-[var(--dim)]">##</span> WHY BOUNDED
        </h2>
        <div className="mt-4 grid gap-px bg-[var(--line)] sm:grid-cols-2">
          {FEATURES.map((f) => (
            <div key={f.k} className="bg-[var(--panel)] p-5">
              <div className="text-[var(--green)]">▸ {f.k}</div>
              <p className="mt-2 text-sm text-[var(--dim)] leading-6">{f.v}</p>
            </div>
          ))}
        </div>
      </section>

      <section className="py-10">
        <h2 className="text-[var(--green)] text-sm tracking-widest">
          <span className="text-[var(--dim)]">##</span> INSTALL
        </h2>
        <div className="frame mt-4 p-4 text-sm leading-7 overflow-x-auto">
          <div>
            <span className="text-[var(--dim)]"># build from source</span>
          </div>
          <div>
            <span className="text-[var(--green)]">$</span> cargo build --release -p agentd
          </div>
          <div className="mt-3">
            <span className="text-[var(--dim)]">
              # or a sealed appliance: no outbound http, no shell — the code isn&apos;t in the
              binary
            </span>
          </div>
          <div>
            <span className="text-[var(--green)]">$</span> cargo build --release -p agentd
            --no-default-features \
          </div>
          <div>
            {"    "}--features &quot;tools-fs,tools-data,trigger-http,auth,server-tls&quot;
          </div>
        </div>
      </section>
    </main>
  );
}
