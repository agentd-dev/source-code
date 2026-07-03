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
    --mcp github=https://mcp-github.internal/mcp \\
    --intelligence https://gateway.internal/v1 \\
    --model claude-sonnet-4-6

{"event":"mcp.connect","server":"github","proto":"2025-11-25"}
{"event":"loop.start","tools":11,"servers":1,"run_id":"19f0…"}
{"event":"tool.call","tool":"list_issues"}
{"event":"tool.call","tool":"add_labels","args":{"labels":["bug"]}}
{"event":"run.exit","status":"completed","steps":4,"exit_code":0}`;

const CAPS = [
  {
    tag: "no local code",
    title: "It runs nothing of its own",
    body: "agentd ships zero tools and executes no code — every capability comes from a remote MCP server you declare. There is no shell, no exec, no plugin. A prompt-injected agent has nothing to break into; the blast radius is exactly the servers you wired.",
  },
  {
    tag: "supervision",
    title: "Two-loop, no orphans",
    body: "A supervisor that never reasons owns lifecycle; the agentic loop runs only inside subagent processes. Dead/stuck detection, a bounded kill ladder, PR_SET_PDEATHSIG, and a restart governor mean a crashed or wedged agent never leaks.",
  },
  {
    tag: "bounded",
    title: "Budgets, by construction",
    body: "Every run is capped by steps, tokens, and a wall-clock deadline; a subagent tree rolls token usage up to one ceiling. Exceed it and the subtree is drained — the agent can spend, but only what you granted.",
  },
  {
    tag: "security",
    title: "Authenticated identity + Rule of Two",
    body: "Trust is a verified mTLS cert or a constant-time bearer — never the transport. Tools are tagged untrusted-input / sensitive / egress; granting one agent all three lethal-trifecta legs is refused at startup unless you override it. Scope narrows monotonically down the tree; secrets are redacted everywhere.",
  },
  {
    tag: "observability",
    title: "Everything is auditable",
    body: "One JSON-lines event stream with run_id + agent_path tree correlation, W3C trace-context propagation, and dependency-free OTLP export. /healthz, /readyz, /metrics for k8s — all opt-in, all off by default.",
  },
  {
    tag: "cloud-native",
    title: "Built for the cluster",
    body: "Terminal statuses map to a documented exit-code contract a podFailurePolicy branches on; SIGTERM drains gracefully to exit 0, not a 143 failure. cgroup-v2 teardown, horizontal sharding + work-claim leases, SIGHUP hot-reload. One static binary, nothing to patch.",
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
    body: "Idle cheaply; wake on a pushed MCP resource update, read it, and act. Event-driven, no polling.",
    k8s: "Deployment",
  },
  {
    k: "loop",
    cmd: "--mode loop",
    body: "Self-paced: run, decide when the next iteration is worth doing, sleep, repeat.",
    k8s: "Deployment",
  },
  {
    k: "workflow",
    cmd: "--mode workflow --workflow",
    body: "Drive an explicit graph of steps — branches, loops, fan-out, waits — the agent authored or an operator pinned. Deterministic where it can be, agentic where it must be.",
    k8s: "Job / Deployment",
  },
];

const SPECS = [
  ["first-party deps", "3 (serde · serde_json · libc)"],
  ["transport", "HTTPS everywhere · rustls + ring · bundled roots"],
  ["runtime", "no async runtime · no C toolchain · blocking I/O + threads"],
  ["binary", "one static musl ELF · stripped · on scratch"],
  ["arch", "amd64 + arm64 · nonroot · read-only rootfs"],
  ["supply chain", "cosign-signed · SPDX SBOM attested"],
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
          loop, calls tools, reads resources, and self-corrects, as a one-shot, a daemon, a reactive
          service, or an agent-authored workflow.
        </p>
        <p className="mt-4 max-w-2xl text-[var(--dim)]">
          MCP-native to the core, over HTTPS: tools come only from remote{" "}
          <span className="text-[var(--green)]">MCP servers</span>, agentd{" "}
          <span className="text-[var(--green)]">is</span> an MCP server, it{" "}
          <span className="text-[var(--green)]">reacts</span> to resource subscriptions, and it{" "}
          <span className="text-[var(--green)]">speaks A2A</span> to other agents. It runs no code
          of its own. One static binary — supervised, bounded, observable.
        </p>

        <div className="mt-7 flex flex-wrap gap-3">
          <a href="#run" className="btn btn-primary">
            $ run it
          </a>
          <a href="https://github.com/agentd-dev/source-code" className="btn">
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
            The task in plain language, the remote MCP servers whose tools it may use, and an
            OpenAI-compatible intelligence endpoint over HTTPS. Capabilities are exactly what you
            wire — no built-in tool zoo, no local execution.
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
        intro="The Model Context Protocol is not an integration in agentd — it is the substrate. Tools, composition, and reactivity all ride one protocol, over Streamable HTTP."
      >
        <div className="grid gap-4 md:grid-cols-3">
          <div className="panel lift p-5">
            <div className="mb-3 font-mono text-xs text-[var(--green)]">01 · consumes</div>
            <h3 className="font-semibold text-[var(--fg-strong)]">Tools come from MCP</h3>
            <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">
              Every tool the agent can call is served by a remote MCP server you declare with{" "}
              <span className="kbd">--mcp name=https://host/mcp</span>. agentd connects over
              Streamable HTTP, negotiates the protocol version, discovers the tools, and offers
              exactly that set to the model. It spawns no process and runs no local code.
            </p>
          </div>
          <div className="panel lift p-5">
            <div className="mb-3 font-mono text-xs text-[var(--green)]">02 · serves</div>
            <h3 className="font-semibold text-[var(--fg-strong)]">agentd is an MCP server</h3>
            <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">
              With <span className="kbd">--serve-mcp https://host:port</span> (mTLS or bearer) it
              speaks MCP back: a peer calls <span className="kbd">subagent.spawn</span> (sync ·
              async · detach · warm), <span className="kbd">subagent.send</span>/
              <span className="kbd">status</span>/<span className="kbd">cancel</span>, and reads{" "}
              <span className="kbd">agent://</span> resources. One agent orchestrates others over
              the same wire.
            </p>
          </div>
          <div className="panel lift p-5">
            <div className="mb-3 font-mono text-xs text-[var(--green)]">03 · reacts</div>
            <h3 className="font-semibold text-[var(--fg-strong)]">Reactive on resources</h3>
            <p className="mt-2 text-sm leading-relaxed text-[var(--dim)]">
              <span className="kbd">--subscribe &lt;uri&gt;</span> and agentd idles until a server
              pushes <span className="kbd">notifications/resources/updated</span> over SSE — then it
              reads the resource and runs, optionally only when a condition holds. Event-driven
              agents, no polling, no glue.
            </p>
          </div>
        </div>

        <div className="mt-6 grid gap-4 lg:grid-cols-2">
          <Term title="give it tools — and let it serve its own">{`# the agent's toolset = the union of its remote MCP servers
$ agentd --instruction "reconcile the inbox" \\
    --mcp fs=https://mcp-fs.internal/mcp \\
    --mcp gh=https://mcp-github.internal/mcp \\
    --serve-mcp https://0.0.0.0:8443 \\
    --serve-client-ca /tls/clients.pem   # ← agentd is now an mTLS MCP server`}</Term>
          <Term title="react to a resource changing">{`# wake on every change to the watched resource
$ agentd --mode reactive \\
    --subscribe inbox:///items/new \\
    --mcp inbox=https://mcp-inbox.internal/mcp \\
    --instruction "classify each new item and route it"

{"event":"trigger.armed","kind":"reactive","subscriptions":1}
{"event":"resource.updated","uri":"inbox:///items/new"}  # ← push → run`}</Term>
        </div>
      </Section>

      {/* ── workflows ────────────────────────────────────────── */}
      <Section
        id="workflows"
        eyebrow="agent-authored workflows"
        title="When one loop isn't the right shape"
        intro="Some work is a graph, not a single reasoning loop. agentd lets the agent build one itself — like LangGraph, but the agent authors and drives the graph, and agentd supervises every node."
      >
        <div className="grid gap-4 md:grid-cols-3">
          <Card tag="deterministic where it can be" title="Nine node kinds">
            <span className="kbd">agent</span>, <span className="kbd">tool</span> (with{" "}
            <span className="kbd">$from</span> data flow), <span className="kbd">assign</span>,{" "}
            <span className="kbd">infer</span> (schema-checked structured extraction),{" "}
            <span className="kbd">branch</span>, <span className="kbd">foreach</span>,{" "}
            <span className="kbd">wait</span>, <span className="kbd">subgraph</span>,{" "}
            <span className="kbd">halt</span>. A tool/branch-only path spends zero model tokens.
          </Card>
          <Card tag="fan out without the model" title="Process arrays at scale">
            A tool returns 500 items? <span className="kbd">foreach</span> maps a body over each on
            up to 8 parallel lanes — deterministically, without feeding the array through the LLM.
            Cross-key predicates and computed pointers keep the routing token-free.
          </Card>
          <Card tag="agentic where it must be" title="Authored, run, or delegated">
            The agent calls <span className="kbd">workflow.define</span> /{" "}
            <span className="kbd">workflow.run</span> mid-reasoning, an operator pins one with{" "}
            <span className="kbd">--mode workflow</span>, or a parent hands a whole workflow to a
            supervised subagent. Layered termination — budget, token pool, deadline, loop + progress
            guards — every stop with a reason.
          </Card>
        </div>
        <div className="mt-6">
          <Term title="a review loop the agent can write itself">{`{ "start": "draft",
  "nodes": {
    "draft":  { "kind": "agent", "instruction": "draft the release note", "writes": "doc",
                "edges": { "ok": "judge", "error": "fail" } },
    "judge":  { "kind": "branch", "cases": [], "default": "revise",
                "semantic": { "prompt": "Is it ready to publish?", "reads": ["doc"],
                              "choices": { "yes": "publish", "no": "revise" } } },
    "revise": { "kind": "agent", "instruction": "revise it", "reads": ["doc"],
                "writes": "doc", "edges": { "ok": "judge", "error": "fail" } },
    "publish":{ "kind": "halt", "status": "completed", "result_from": "doc" },
    "fail":   { "kind": "halt", "status": "crashed" } } }
# feature-gated (--features workflow); optional CEL for expression predicates.`}</Term>
        </div>
      </Section>

      {/* ── capabilities ─────────────────────────────────────── */}
      <Section
        id="capabilities"
        eyebrow="capabilities"
        title="Small surface, serious guarantees"
        intro="agentd is minimal where it can be and uncompromising where it must be — no local execution, supervision, budgets, authenticated control, and observability are not add-ons."
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
        title="One binary, five shapes"
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
          And within a run, an agent can <span className="text-[var(--fg)]">spawn subagents</span>{" "}
          (sync · async · detach · warm), <span className="text-[var(--fg)]">delegate a whole
          workflow</span> to a child, or <span className="text-[var(--fg)]">delegate over A2A</span>{" "}
          to another agent entirely. Depth and breadth are bounded; the whole tree is one reaping
          domain. Operators drive a running instance over the same HTTPS surface —{" "}
          <span className="kbd">a2a.Drain</span> / <span className="kbd">Pause</span> /{" "}
          <span className="kbd">Cancel</span>, authenticated, never a plaintext control plane.
        </p>
      </Section>

      {/* ── A2A ──────────────────────────────────────────────── */}
      <Section
        id="a2a"
        eyebrow="agent-to-agent"
        title="A first-class agent in the mesh"
        intro="agentd speaks the Agent2Agent protocol both ways — a served run is an A2A Task — so it interoperates with any conformant A2A peer, not just other agentds."
      >
        <div className="grid gap-4 md:grid-cols-2">
          <Card tag="as a server" title="Your agent, callable">
            A peer sends <span className="kbd">SendMessage</span> /{" "}
            <span className="kbd">SendStreamingMessage</span>, polls{" "}
            <span className="kbd">GetTask</span>, and streams status + artifact updates over SSE —
            the A2A spec's JSON-RPC binding, verbatim. mTLS/bearer-gated.
          </Card>
          <Card tag="as a client" title="It delegates outward">
            Declare a peer with <span className="kbd">--a2a-peer</span> and the agent can hand an
            objective to another A2A agent mid-reasoning, streaming-first with graceful recovery.
            One protocol for the whole mesh.
          </Card>
        </div>
        <p className="mt-4 text-sm text-[var(--dim)]">
          The full method surface — <span className="kbd">SendMessage</span> ·{" "}
          <span className="kbd">SendStreamingMessage</span> · <span className="kbd">GetTask</span> ·{" "}
          <span className="kbd">CancelTask</span> · <span className="kbd">ListTasks</span> ·{" "}
          <span className="kbd">SubscribeToTask</span> — with spec-exact semantics:{" "}
          <span className="text-[var(--fg)]">blocking by default</span> (opt into{" "}
          <span className="kbd">returnImmediately</span>), spec error codes
          (TaskNotFound, TaskNotCancelable, UnsupportedOperation), and terminality signalled by
          the task state + stream close. A run's lifecycle <em>is</em> the Task lifecycle — no
          adapter layer, no second state machine.
        </p>
      </Section>

      {/* ── cloud-native spec sheet ──────────────────────────── */}
      <Section
        eyebrow="footprint"
        title="Minimalism is the moat"
        intro="Three first-party dependencies. The only other code in the build is rustls + ring for the HTTPS transport — no async runtime, no framework, no C toolchain. It links statically and ships on an empty base."
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
COPY agentd /agentd        # one static musl binary
USER 65532:65532           # nonroot
ENTRYPOINT ["/agentd"]

# no shell · no libc · no package manager · nothing to attack or patch
# HTTPS by default (rustls + bundled roots) — dial https:// with no CA bundle
# opt-in k8s probes: --metrics-addr :9090 → /healthz /readyz /metrics
# opt out of TLS (--no-default-features) for a loopback sidecar posture`}</Term>
        </div>
      </Section>

      {/* ── run it ───────────────────────────────────────────── */}
      <Section
        id="run"
        eyebrow="quickstart"
        title="Run it"
        intro="Pull the image, or build from source. Point it at a remote MCP server and a model over HTTPS, and go."
      >
        <div className="grid gap-4 lg:grid-cols-2">
          <Term title="docker">{`$ docker run --rm ghcr.io/agentd-dev/agentd \\
    --instruction "summarize /data/report.txt and write a digest" \\
    --mcp fs=https://mcp-fs.internal/mcp \\
    --intelligence https://gateway.internal/v1 \\
    --model claude-sonnet-4-6`}</Term>
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
          args: ["--mcp", "fs=https://mcp-fs.internal/mcp"]
          env:
            - { name: INSTRUCTION, value: "digest the report" }
            - { name: AGENT_INTELLIGENCE, value: "https://gw/v1" }
      # podFailurePolicy maps agentd's exit codes → retriable vs terminal`}</Term>
        </div>
        <div className="mt-6 flex flex-wrap items-center gap-3 text-sm">
          <a href="https://github.com/agentd-dev/source-code" className="btn btn-primary">
            star on github ↗
          </a>
          <Link href="/docs/overview/" className="btn">
            read the docs
          </Link>
          <Link href="/docs/workflows/" className="btn">
            workflows
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
