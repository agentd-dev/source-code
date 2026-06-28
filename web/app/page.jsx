import Link from "next/link";

/* ── tiny presentational helpers ─────────────────────────────────── */

function Term({ title = "shell", children }) {
  return (
    <div className="term">
      <div className="panel-title">
        <span className="dots">
          <i />
          <i />
          <i />
        </span>
        <span className="ml-1">{title}</span>
      </div>
      <pre>{children}</pre>
    </div>
  );
}

function Section({ id, eyebrow, title, intro, children }) {
  return (
    <section id={id} className="mx-auto max-w-5xl scroll-mt-20 px-4 py-16">
      {eyebrow && <div className="eyebrow mb-3">{eyebrow}</div>}
      {title && (
        <h2 className="text-2xl font-bold text-[var(--fg-strong)] sm:text-3xl">{title}</h2>
      )}
      {intro && <p className="mt-3 max-w-2xl text-[var(--dim)]">{intro}</p>}
      <div className="mt-8">{children}</div>
    </section>
  );
}

function Card({ tag, title, children }) {
  return (
    <div className="panel lift p-5">
      {tag && <div className="mb-2 text-xs text-[var(--green)]">{tag}</div>}
      <h3 className="text-[15px] font-semibold text-[var(--fg-strong)]">{title}</h3>
      <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">{children}</p>
    </div>
  );
}

/* ── content ─────────────────────────────────────────────────────── */

const HERO_CMD = `$ agentd \\
    --instruction "triage new GitHub issues and label them" \\
    --mcp "github=mcp-server-github" \\
    --intelligence unix:/run/intel.sock \\
    --model claude-sonnet-4-6

{"event":"loop.start","tools":11,"servers":1,"run_id":"19f0…"}
{"event":"tool.call","tool":"list_issues"}
{"event":"tool.call","tool":"add_labels","args":{"labels":["bug"]}}
{"event":"loop.final","status":"completed","steps":4}`;

const CAPS = [
  {
    tag: "supervision",
    title: "Two-loop, no orphans",
    body: "A supervisor that never reasons owns lifecycle; the agentic loop runs only inside subagent processes. Dead/stuck detection, a bounded kill ladder, PR_SET_PDEATHSIG, and a restart governor mean a crashed or wedged agent never leaks.",
  },
  {
    tag: "bounded",
    title: "Budgets, by construction",
    body: "Every run is capped by steps, tokens, and a wall-clock deadline; a subagent tree rolls token usage up to a ceiling. Exceed it and the subtree is drained — the agent can spend, but only what you granted.",
  },
  {
    tag: "security",
    title: "Rule of Two",
    body: "Tools are tagged untrusted-input / sensitive / egress; granting one agent all three lethal-trifecta legs is refused at startup unless you explicitly override it. Scope narrows monotonically down the subagent tree. Secrets are redacted from every log and payload.",
  },
  {
    tag: "observability",
    title: "Everything is auditable",
    body: "One JSON-lines event stream with run_id + agent_path tree correlation, W3C trace-context propagation, and dependency-free OTLP export. /healthz, /readyz, /metrics for k8s — all opt-in, all off by default.",
  },
  {
    tag: "composition",
    title: "Agents that compose",
    body: "A running agentd spawns subagents — sync, async, detached, or warm (multi-turn) — and awaits or messages them. Completions are agent:// resources a parent can read or subscribe to. Orchestration is just more MCP.",
  },
  {
    tag: "cloud-native",
    title: "Built for the cluster",
    body: "Terminal statuses map to a documented exit-code table; SIGTERM drains gracefully to exit 0, not a 143 failure. cgroup-v2 teardown + memory.max/pids.max where delegated. One static binary, nothing to patch.",
  },
];

const MODES = [
  {
    k: "once",
    cmd: "--mode once",
    body: "Run an instruction to a terminal status and exit with a cloud-native code. The unit of work is the instruction.",
    k8s: "Kubernetes Job",
  },
  {
    k: "schedule",
    cmd: "--mode schedule --cron",
    body: "Fire on a 5-field UTC cron (or an interval). Hand-rolled, zero-dependency.",
    k8s: "CronJob",
  },
  {
    k: "reactive",
    cmd: "--mode reactive --subscribe",
    body: "Idle cheaply; wake on an MCP resource update, read it, and act. Event-driven, no polling.",
    k8s: "Deployment",
  },
  {
    k: "loop",
    cmd: "--mode loop",
    body: "Self-paced: run, decide when the next iteration is worth doing, sleep, repeat.",
    k8s: "Deployment",
  },
];

const SPECS = [
  ["binary", "~1.3 MB · static musl · stripped"],
  ["image", "~0.65 MB pull · on scratch · amd64 + arm64"],
  ["dependencies", "3 (serde · serde_json · libc)"],
  ["runtime", "no async runtime · no TLS · no C toolchain"],
  ["supply chain", "cosign-signed · SPDX SBOM attested"],
  ["k8s", "Job · CronJob · Deployment · exit-code policy · httpGet probes"],
];

export default function Home() {
  return (
    <main>
      {/* ── hero ─────────────────────────────────────────────── */}
      <section className="mx-auto max-w-5xl px-4 pt-16 pb-10 sm:pt-24">
        <div className="chip mb-6">
          <span className="pulse" /> a runtime, not a framework
        </div>
        <h1 className="text-4xl font-bold leading-tight tracking-tight text-[var(--fg-strong)] sm:text-6xl">
          agentd<span className="cursor" aria-hidden="true" />
        </h1>
        <p className="mt-5 max-w-2xl text-lg text-[var(--fg)] sm:text-xl">
          A small, cloud-native AI agent runtime. Give it an{" "}
          <span className="text-[var(--fg-strong)]">instruction</span> and{" "}
          <span className="text-[var(--fg-strong)]">tools from MCP</span> — it runs the agentic
          loop, calls tools, reads resources, and self-corrects, as a one-shot, a daemon, or a
          reactive service.
        </p>
        <p className="mt-4 max-w-2xl text-[var(--dim)]">
          Minimal and MCP-native to the core: tools come only from{" "}
          <span className="text-[var(--green)]">MCP servers</span>, agentd{" "}
          <span className="text-[var(--green)]">is</span> an MCP server, and it{" "}
          <span className="text-[var(--green)]">reacts</span> to MCP resource subscriptions. One
          static binary — supervised, bounded, observable.
        </p>

        <div className="mt-7 flex flex-wrap gap-3">
          <a href="#run" className="btn btn-primary">
            $ run it
          </a>
          <a
            href="https://github.com/agentd-dev/source-code"
            className="btn"
          >
            github ↗
          </a>
          <Link href="/docs/overview/" className="btn">
            docs
          </Link>
        </div>

        <div className="mt-10">
          <Term title="agentd — once mode">{HERO_CMD}</Term>
        </div>
      </section>

      {/* ── the model ────────────────────────────────────────── */}
      <Section
        eyebrow="the model"
        title="An instruction, some tools, one loop"
        intro="agentd is deliberately small. You give it three things; it does one thing well and tells you exactly what happened."
      >
        <div className="grid gap-4 md:grid-cols-3">
          <Card tag="you provide" title="An instruction + MCP servers + a model">
            The task in plain language, the MCP servers whose tools it may use, and an
            OpenAI-compatible intelligence endpoint. Capabilities are exactly what you wire — no
            built-in tool zoo.
          </Card>
          <Card tag="it runs" title="The ReAct loop, supervised">
            Think → call a tool over MCP → observe the result → repeat, until it has an answer or
            hits a budget. The loop lives inside a subagent process; a supervisor with no model
            owns its lifecycle.
          </Card>
          <Card tag="it ends" title="A terminal status + a trace">
            A completed / partial / refused / budget-exceeded outcome, mapped to an exit code — or
            it stays alive as a reactive daemon. Either way, every step is on the event stream.
          </Card>
        </div>
      </Section>

      {/* ── MCP-native ───────────────────────────────────────── */}
      <Section
        id="mcp"
        eyebrow="model context protocol"
        title="MCP-native, three ways"
        intro="The Model Context Protocol is not an integration in agentd — it is the substrate. Tools, composition, and reactivity all ride one protocol."
      >
        <div className="grid gap-4 md:grid-cols-3">
          <div className="panel lift p-5">
            <div className="mb-3 font-mono text-xs text-[var(--green)]">01 · consumes</div>
            <h3 className="font-semibold text-[var(--fg-strong)]">Tools come from MCP</h3>
            <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">
              Every tool the agent can call is exposed by an MCP server you declare with{" "}
              <span className="kbd">--mcp name=cmd</span>. agentd connects over stdio, discovers
              the tools, and offers exactly that set to the model. Want filesystem access? Wire an
              fs server. Want none? Wire none.
            </p>
          </div>
          <div className="panel lift p-5">
            <div className="mb-3 font-mono text-xs text-[var(--green)]">02 · serves</div>
            <h3 className="font-semibold text-[var(--fg-strong)]">agentd is an MCP server</h3>
            <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">
              With <span className="kbd">--serve-mcp</span> it speaks MCP back: a peer calls{" "}
              <span className="kbd">subagent.spawn</span> (sync · async · detach · warm),{" "}
              <span className="kbd">subagent.send</span>/<span className="kbd">status</span>/
              <span className="kbd">cancel</span>, and reads{" "}
              <span className="kbd">agent://</span> resources. One agent orchestrates others over
              the same wire.
            </p>
          </div>
          <div className="panel lift p-5">
            <div className="mb-3 font-mono text-xs text-[var(--green)]">03 · reacts</div>
            <h3 className="font-semibold text-[var(--fg-strong)]">Reactive on resources</h3>
            <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">
              <span className="kbd">--subscribe &lt;uri&gt;</span> and agentd idles until the
              server pushes <span className="kbd">notifications/resources/updated</span> — then it
              reads the resource and runs. Event-driven agents with no polling, no glue.
            </p>
          </div>
        </div>

        <div className="mt-6 grid gap-4 lg:grid-cols-2">
          <Term title="give it tools — and let it serve its own">{`# the agent's toolset = the union of its MCP servers
# (quote each name=command so agentd passes the flags to the server)
$ agentd --instruction "reconcile the inbox" \\
    --mcp "fs=mcp-server-fs --root /data" \\
    --mcp "gh=mcp-server-github" \\
    --serve-mcp unix:/run/agentd.sock      # ← now agentd is itself an MCP server`}</Term>
          <Term title="react to a resource changing">{`# wake on every change to the watched resource
$ agentd --mode reactive \\
    --subscribe file:///data/inbox \\
    --mcp "fs=mcp-server-fs --root /data" \\
    --instruction "classify each new item and route it"

{"event":"trigger.armed","kind":"reactive","subscriptions":1}
{"event":"react","uri":"file:///data/inbox"}   # ← resource updated → run`}</Term>
        </div>
      </Section>

      {/* ── capabilities ─────────────────────────────────────── */}
      <Section
        id="capabilities"
        eyebrow="capabilities"
        title="Small surface, serious guarantees"
        intro="agent is minimal where it can be and uncompromising where it must be — supervision, budgets, security, and observability are not add-ons."
      >
        <div className="grid gap-4 md:grid-cols-2 lg:grid-cols-3">
          {CAPS.map((c) => (
            <Card key={c.title} tag={c.tag} title={c.title}>
              {c.body}
            </Card>
          ))}
        </div>
      </Section>

      {/* ── modes / orchestration ────────────────────────────── */}
      <Section
        eyebrow="run shapes"
        title="One binary, four shapes"
        intro="The same loop, the same config — only the lifecycle differs. Each maps cleanly onto a Kubernetes primitive."
      >
        <div className="panel overflow-hidden">
          {MODES.map((m, i) => (
            <div
              key={m.k}
              className={
                "grid grid-cols-1 gap-2 px-5 py-4 sm:grid-cols-12 sm:items-center " +
                (i ? "border-t border-[var(--line)]" : "")
              }
            >
              <div className="sm:col-span-2">
                <span className="text-[var(--fg-strong)]">{m.k}</span>
              </div>
              <div className="font-mono text-xs text-[var(--green)] sm:col-span-3">{m.cmd}</div>
              <div className="text-sm text-[var(--dim)] sm:col-span-5">{m.body}</div>
              <div className="text-xs text-[var(--dim)] sm:col-span-2 sm:text-right">
                <span className="text-[var(--dimmer)]">→</span> {m.k8s}
              </div>
            </div>
          ))}
        </div>
        <p className="mt-4 text-sm text-[var(--dim)]">
          And within a run, an agent can{" "}
          <span className="text-[var(--fg)]">spawn subagents</span> — delegating a narrowed task to
          a child it supervises, awaits, or keeps warm across turns. Depth and breadth are bounded;
          the whole tree is one reaping domain.
        </p>
      </Section>

      {/* ── cloud-native spec sheet ──────────────────────────── */}
      <Section
        eyebrow="footprint"
        title="Minimalism is the moat"
        intro="No async runtime, no TLS in the default build, no C toolchain — just serde + libc. It links statically and ships on an empty base."
      >
        <div className="grid gap-4 lg:grid-cols-2">
          <div className="panel divide-y divide-[var(--line)]">
            {SPECS.map(([k, v]) => (
              <div key={k} className="flex items-center justify-between gap-4 px-5 py-3.5">
                <span className="text-xs uppercase tracking-wider text-[var(--dim)]">{k}</span>
                <span className="text-right text-sm text-[var(--fg)]">{v}</span>
              </div>
            ))}
          </div>
          <Term title="the whole image">{`FROM scratch
COPY agentd /agentd        # one static musl binary, ~1.3 MB
USER 65532:65532           # nonroot
ENTRYPOINT ["/agentd"]

# no shell · no libc · no package manager · nothing to attack or patch
# opt-in k8s probes: --metrics-addr :9090 → /healthz /readyz /metrics
# TLS-free by default — reach the model over unix: to a sidecar
# (build the :tls variant to dial https:// directly)`}</Term>
        </div>
      </Section>

      {/* ── run it ───────────────────────────────────────────── */}
      <Section
        id="run"
        eyebrow="quickstart"
        title="Run it"
        intro="Pull the image, or build from source. Point it at an MCP server and a model, and go."
      >
        <div className="grid gap-4 lg:grid-cols-2">
          <Term title="docker">{`$ docker run --rm ghcr.io/agentd-dev/agentd \\
    --instruction "summarize /data/report.txt and write a digest" \\
    --mcp "fs=mcp-server-fs --root /data" \\
    --intelligence unix:/run/intel.sock --model claude-sonnet-4-6`}</Term>
          <Term title="kubernetes — a one-shot Job">{`apiVersion: batch/v1
kind: Job
metadata: { name: agentd-digest }
spec:
  template:
    spec:
      restartPolicy: Never
      containers:
        - name: agentd
          image: ghcr.io/agentd-dev/agentd:latest
          args: ["--mcp", "fs=mcp-server-fs --root /data"]
          env:
            - { name: INSTRUCTION, value: "digest the report" }
      # podFailurePolicy maps agentd's exit codes → retriable vs terminal`}</Term>
        </div>
        <div className="mt-6 flex flex-wrap items-center gap-3 text-sm">
          <a href="https://github.com/agentd-dev/source-code" className="btn btn-primary">
            star on github ↗
          </a>
          <Link href="/docs/overview/" className="btn">
            read the docs
          </Link>
          <Link href="/docs/mcp/" className="btn">
            mcp surface
          </Link>
          <span className="text-[var(--dim)]">
            examples for Job · CronJob · Deployment in{" "}
            <span className="kbd">examples/k8s/</span>
          </span>
        </div>
      </Section>
    </main>
  );
}
