# RFC 0046: Kinds of intelligence — chat, reasoning and decision models, by role

**Status:** Accepted 2026-10-10 (Q1–Q3 decided, §11); P1–P4 to be built in order
**Author:** Andrii Tsok (drafted with Claude)
**Date:** 2026-10-10
**Depends on:** RFC 0031 (endpoint authentication: every provider is signed and credentialed like `intelligence.endpoints` is today), RFC 0037 (service catalog and egress: every provider endpoint is an egress destination), RFC 0042 (what a served document may configure), RFC 0038 (the prompt template).
**Replaces:** `intelligence.default`, `intelligence.preflight_model` and `context.summarize.model`, which become roles (§5.4). Nothing else is removed.

Sources (read 2026-10-10):

- **[JEV-OR]** OpenRouter, *What is Jev?* — the request and response shape: https://openrouter.ai/blog/insights/what-is-jev/
- **[JEV-SW]** Simon Willison, *Jev introduces a new shape of LLM — System One, aka Decision Models* (2026-09-21): https://simonwillison.net/2026/Sep/21/jev/
- **[OAI-DEC]** OpenAI Decisions API, public beta 2026-10-06, `POST /v1/decisions`, one model (`gpt-6-luna`); the request schema is not published yet: https://community.openai.com/t/decisions-api-is-now-available-in-public-beta/1403877
- **[STRANDS]** AWS, *Introducing Strands Decider 2B* (open weights, served at `/v1/systemone`, same `state` + `questions` shape): https://strandsagents.com/blog/introducing-strands-decider/
- **[ANYJEV]** Nokia Applied Research, AnyJev — any LLM as a Jev-style decision model, probabilities from logprobs: https://github.com/nokia-applied-research/AnyJev ; and *LLM-as-Jev* (arXiv 2610.02076)
- **[CP]** Check Point, *A decision model breaks like any other language model* — prompt injection against Jev: https://blog.checkpoint.com/ai-security/jev-is-not-a-language-model-but-it-breaks-like-one-prompt-injection-against-a-typed-decision-model/ ; and *JevOut: natural context can flip decision models* (arXiv 2609.30243)

agentd paths are relative to `crates/agentd/src`. Code references are to `main` at `180f605f`.

---

## 1. Summary

agentd talks to one kind of model: a chat model, behind one provider (`intelligence.endpoints`), and every model call it makes — the agent's turns, preflight triage, compaction, the goal check, the `auto` ask judge, every workflow node — is a chat completion. Choosing a model per job is possible but scattered across five knobs, and every *decision* agentd asks a model for (which intent, which route, pass or fail, approve or not) is a chat call asked to emit JSON with a self-reported confidence.

Three kinds of model now matter, and they are not interchangeable:

| Kind | What it is | Contract |
|---|---|---|
| `chat` | a generative model: text, tool calls, structured JSON. Large or small — size is a choice of model, not a kind. | **generate** |
| `reasoning` | a chat model that thinks before it answers, with an effort or token budget for it, and provider rules that differ from plain chat (no temperature, reasoning blocks carried across tool rounds). | **generate**, with reasoning controls |
| `decision` | a "System One" model ([JEV-SW]): text or JSON in, typed answers out — a choice among options, a yes/no probability, a score on a scale — with calibrated probabilities and confidence, and no prose at all. Jev (TypeSafe, 2026-09-15), OpenAI's Decisions API (beta 2026-10-06), Strands Decider 2B (open weights). | **decide** |

This RFC makes the kind explicit and lets every place agentd uses intelligence say which model — and so which kind — does the job:

1. **Providers.** `intelligence.providers` names further connections beside the top-level one, each with its own endpoints, dialect and credential — a decision model is almost always behind a different endpoint than the chat model. Operator-only, egress-checked, like `intelligence.endpoints` today (§5.2).
2. **Models.** Each entry under `intelligence.models` gains `kind` (`chat` default, `reasoning`, `decision`) and `provider`. A reasoning model carries its `reasoning` defaults; a decision model may name an `escalate` partner (§5.3).
3. **Roles.** `intelligence.roles` binds each job agentd does on its own to a model: `turn`, `think`, `decide`, `preflight`, `compaction`, `goal`, `approve`. Each role says which kinds it accepts; load refuses a mismatch (§5.4).
4. **Two contracts.** Every call is *generate* or *decide*. A decision model serves only *decide*; a chat or reasoning model serves both, *decide* by emulation (one structured call, `calibrated: false`). So every decision point works with whatever model is bound to it, and gets better when a decision model is (§5.5).
5. **Confidence and escalation.** A decision carries its confidence; a use site may set a threshold, and an answer under it is re-asked on the decision model's `escalate` model (System One, then System Two), or handled by the use site's `on_unsure` (§5.6).
6. **Workflows.** `model:` on any intelligence node names any model its kind can serve; `classify`, `route` and `judge` run natively on a decision model; a new `decide` node asks several typed questions about one state in one call; `think` and `agent` nodes take `effort:` for a reasoning model (§6).
7. **In the loop.** The agent's turn keeps its model, and can reach the other kinds through the tools it already has: `think` runs on the `think` role (a reasoning model — System Two on demand), and a new `decide` tool asks the `decide` role for typed, calibrated answers (§7).

Behaviour with today's configuration is unchanged: an unset role is the turn's model, and the turn's model is `intelligence.model`.

## 2. Motivation

**Decisions are most of what an agent runtime asks a model.** Look at what agentd asks for on its own: preflight is a choice of intent, a yes/no on whether a plan is needed and a choice of risk (`runtime/turns.rs:533`); the goal check is achieved-or-not and stuck-or-not (`runtime/goal.rs:310`); a policy gate under `ask_human_fallback: auto` is approve-or-not (`runtime/human.rs:1089`); `classify`, `route` and `judge` nodes are a choice, a choice and a verdict (`runtime/waits.rs:1951`). Each runs today as a chat completion told to "reply with ONLY a JSON object", with a confidence the model makes up. A decision model answers the same question for a fraction of the cost ([JEV-SW]: input-only billing at $0.042 per million tokens), in one fast call however many questions it holds, with probabilities an application can threshold on, and with no free text to parse or to hallucinate.

**Reasoning models are a different contract, not just a better model.** They take an effort or a thinking budget; several providers refuse `temperature` with them; Anthropic requires the signed thinking blocks of a turn to be sent back unchanged while that turn is still calling tools. agentd today cannot send any of that, so a reasoning model behind agentd either runs with its provider's default effort or fails on a field agentd always sends.

**Small models need no new kind — they need to be choosable per job.** That exists (tiers, `models:`), but which tier does which job is spread over `intelligence.default`, `intelligence.preflight_model`, `context.summarize.model`, `subagents.defaults.model`, `budget.degrade.model` and per-node `model:`, and the goal check and the ask judge cannot be given one at all — they always use the turn's model (`runtime/goal.rs:201`, `runtime/human.rs:906`).

**And one provider is not enough.** A tier deliberately has no endpoint (`config/settings/mod.rs:1543-1558`): "make it cheaper" must not become a new destination or credential. That was right, and stays right for a *tier*. But Jev is reached through OpenRouter or TypeSafe, OpenAI's decisions through `/v1/decisions`, Strands Decider through a SageMaker or local endpoint — none of them behind the chat model's endpoint. Supporting them means more than one provider; doing it safely means providers stay operator-declared, credentialed and egress-checked exactly as `intelligence.endpoints` is, and a model can only *name* one.

## 3. The field (2026-10)

The decision-model shape has converged within a month of Jev's release. [JEV-OR] documents it:

```json
{ "model": "typesafe/jev-1.13",
  "state": "a string, an array of strings, or a JSON object",
  "questions": {
    "queue":  { "type": "choice", "instructions": "Which team handles this?",
                "criteria": { "billing": "payment problems", "tech": "bugs and outages" } },
    "urgent": { "type": "noul",   "instructions": "The customer cannot work at all." },
    "tone":   { "type": "score",  "instructions": "How upset is the customer?",
                "criteria": [ "calm", "annoyed", "angry" ] } } }
```

```json
{ "answers": {
    "queue":  { "type": "choice", "choice": "tech", "probabilities": { "billing": 0.08, "tech": 0.92 }, "confidence": 0.84 },
    "urgent": { "type": "noul",   "noul": 0.71 },
    "tone":   { "type": "score",  "score": 1.15, "probabilities": { "0": 0.1, "1": 0.65, "2": 0.25 }, "confidence": 0.77,
                "legend": { "0": "calm", "1": "annoyed", "2": "angry" } } },
  "usage": { "input_tokens": 447, "output_tokens": 69 } }
```

Strands Decider 2B serves the same `state` + `questions` body at `/v1/systemone` [STRANDS]; OpenRouter at `/api/alpha/decisions`; OpenAI at `/v1/decisions`, whose request schema is not published yet [OAI-DEC]. The paths differ; the body does not. Limits differ by vendor (Jev: a 32k-token state; one independent endpoint: 1–6 questions, 2–8 options, 2–10 levels). AnyJev and *LLM-as-Jev* [ANYJEV] show a chat model can answer the same questions, with probabilities from logprobs where the provider exposes them. And [CP] shows decision models are prompt-injectable like any language model: text in the state can flip the answer. §8 takes that seriously.

## 4. agentd today

- **One provider.** `Intelligence` (`config/settings/mod.rs:1524`) holds one endpoint list, one dialect (`openai`, `anthropic`, `bedrock`), one credential. The runtime holds one connection (`runtime/reactor.rs:449` `intel_uri`, plus headers, bearer, signer, dialect), and every call — turn workers, the goal judge, the ask judge — is built from it.
- **Tiers.** `intelligence.models.<name>: {model, window, fallback}` (`:1571`); `wire_model` resolves a reference, tier first, literal otherwise (`:1591`); `fallback` is the degradation ladder. A tier has no endpoint, by design.
- **Model choice per job:** `intelligence.default` (turns), `intelligence.preflight_model`, `context.summarize.model` (compaction), node `model:` on `think`, `agent`, `classify`, `extract`, `summarize`, `judge`, `route` (`engine/model.rs:595-660`; the presets carry it into their `think`, `runtime/waits.rs:2066`), `subagents.defaults.model` and template `model:`, `budget.degrade.model`. The goal check and the ask judge always use the turn model.
- **Decisions are prompted.** `classify`/`route`/`judge` are `think` presets with a fixed prompt and a JSON schema (`runtime/waits.rs:1951`); preflight is a `think` with `preflight_schema` (`runtime/turns.rs:533`); the goal and ask judges parse prose (`runtime/goal.rs:363`, `runtime/human.rs:1089`).
- **The `think` tool** is already the agent's "reasoning module" (`runtime/tools.rs:1259`): a tool-less child turn with an optional output schema — on the turn's model.
- **What a document may set:** `intelligence.model`, `.models`, `.default`, `.preflight_model`, `.budget` … — *which* model, never where it lives or the credential (`config/settings/mod.rs:1007-1017`); `intelligence.endpoints`, `.token`, `.headers`, `.auth` are operator-only (`:1146-1151`). All of `intelligence` is reloadable (`:8490`).

## 5. Design

### 5.1 Two contracts, three kinds

Every model call agentd makes is one of two contracts:

- **generate** — messages, optionally tools and an output schema, in; text, tool calls or a JSON value out. The agent loop, `agent` and `think` nodes, `extract`, `summarize`, compaction, an open question to the ask judge.
- **decide** — a *state* (text or JSON) and named typed *questions* in; one typed answer per question out:
  - `choice` — one of named options, with `probabilities` over all of them and a `confidence`;
  - `noul` — a statement, answered with the probability it is true;
  - `score` — ordered levels, answered with a position on the scale, `probabilities` per level and a `confidence`.

| Kind | generate | decide |
|---|---|---|
| `chat` | yes | emulated: one structured call per decision, every question in it; `probabilities` only where the dialect returns logprobs, else absent; `calibrated: false` |
| `reasoning` | yes, with reasoning controls (§5.7) | emulated, as `chat` |
| `decision` | **no** — refused at load wherever generation is needed | native (§5.5) |

Size is not a kind: a small model is a `chat` model a role or a node names. Embedding models are out of scope; nothing in agentd consumes vectors yet (§11).

### 5.2 Providers

```yaml
intelligence:
  # The top-level connection, unchanged — the short form still works alone.
  endpoints: https://api.anthropic.com
  dialect: anthropic
  token: "{{secret:ANTHROPIC_API_KEY}}"
  model: claude-sonnet-5

  # Further connections, by name.
  providers:
    openrouter:
      endpoints: https://openrouter.ai/api/alpha/decisions
      dialect: decisions
      token: "{{secret:OPENROUTER_API_KEY}}"
    decider:                                  # Strands Decider 2B on SageMaker
      endpoints: https://runtime.sagemaker.eu-west-1.amazonaws.com/endpoints/decider/invocations
      dialect: decisions
      auth: { kind: aws, service: sagemaker }
```

- A provider's vocabulary is the top-level connection's: `endpoints` (a failover list), `dialect`, `token` / `token_file`, `headers`, `auth`, `timeout`. Nothing else.
- `dialect: decisions` is new: the endpoint is the **full URL** of the decisions resource, posted as is, because vendors put it at different paths (§3). The chat dialects keep appending their path to a base URL.
- Every provider endpoint is egress-checked against `services` entries of `kind: intelligence` exactly as `intelligence.endpoints` is (`config/settings/mod.rs:7105`); under `egress: closed` an uncatalogued provider refuses the load.
- Each provider gets its own client, breakers, failover and credential refresh. `agentd login intelligence` gains a provider argument (`agentd login intelligence openrouter`) for an `oauth2` provider.
- **Operator-only**, as `intelligence.endpoints` is: a served document cannot add a provider, nor declare a model on one (§5.3).
- Reloadable, as all of `intelligence` is; a changed provider builds a new client at the reload's commit.

### 5.3 Models

```yaml
  models:
    smart:  { model: claude-opus-5-5, window: 1000000, fallback: fast }           # kind: chat, the top-level provider
    fast:   { model: claude-haiku-4-5 }
    deep:   { kind: reasoning, model: claude-opus-5-5, reasoning: { effort: high }, fallback: smart }
    jev:    { kind: decision, provider: openrouter, model: typesafe/jev-1.13, escalate: smart, fallback: fast }
```

| Field | Meaning |
|---|---|
| `kind` | `chat` (default), `reasoning`, `decision` |
| `provider` | a name under `providers`; absent = the top-level connection |
| `model` | the wire model, as today |
| `window` | the context window, as today; meaningless on `decision` (refused) |
| `fallback` | the model used when this one is unavailable or the budget is squeezed — as today, now kind-checked: every model on a chain must serve every use the head is bound to (a `decision` model's fallback may be `chat`, which emulates; a `chat` model's may not be `decision`) |
| `reasoning` | `reasoning` only: `{effort: low\|medium\|high, budget: <tokens>}`, the defaults a use site may override (§5.7) |
| `escalate` | `decision` only: the model an unsure answer is re-asked on (§5.6); must serve *decide* |

A reference still resolves tier-first, literal-second (`wire_model`); a literal reference is a `chat` model on the top-level provider, as today.

**What a document may write.** A served document may declare and name models (it can today), but a model it declares cannot name a `provider` and cannot be `kind: decision` unless an operator-declared model of the same name exists — so a document chooses among what the operator connected, and can never point the agent's content at a new destination. Checked where a document's configuration folds, beside `events::document_declared` (`config/settings/events.rs`).

### 5.4 Roles

```yaml
  roles:
    turn: smart          # the agent's turns, agent nodes, flat subagents        chat | reasoning
    think: deep          # the think tool and think nodes                        chat | reasoning
    decide: jev          # classify/route/judge/decide nodes, the decide tool    any
    preflight: jev       # triage of an inbound message                          any
    compaction: fast     # summarising the transcript                            chat | reasoning
    goal: jev            # the goal check: achieved? stuck?                      any
    approve: { model: jev, confidence: 0.9 }   # asks answered on the operator's behalf (auto)   any; operator-only
```

A role is a model name, or `{model, confidence}` where `confidence` is the threshold its decisions are acted on at (§5.6).

- **Every role defaults to `turn`**, and `turn` defaults to `intelligence.model`. No role inherits from another, so binding `decide` to a decision model changes the nodes that decide and nothing else; preflight, the goal check and approvals move only when their own role is set.
- `turn` and `intelligence.model` are one setting said two ways: `--model`/`MODEL` keep working, and setting both is refused ("say it once").
- **`approve` is operator-only.** The model that answers on the operator's behalf is the operator's choice; a served document must not pick the most agreeable judge for its own gates. Every other role is document-writable, as `default` and `preflight_model` are today.
- **Removed, not aliased** (agentd has no compatibility layer): `intelligence.default` → `roles.turn`; `intelligence.preflight_model` → `roles.preflight`; `context.summarize.model` → `roles.compaction`. An old key is an unknown key at load.
- `subagents.defaults.model`, a template's `model:` and `budget.degrade.model` stay where they are (they qualify a spawn and a budget, not a job), and are kind-checked: `chat | reasoning`.

What each role does with each kind:

| Role | `chat` / `reasoning` | `decision` |
|---|---|---|
| `preflight` | today's `think` with `preflight_schema` | the typed part as one decision: `intent` (choice), `needs_plan` (noul), `risk` (choice), one noul per catalogued skill ("this message calls for the skill: …"). The plan and the clarifications are generated, so they move into the turn: `needs_plan` true tells the turn to make one with `plan.create`. |
| `goal` | today's judge | `achieved` and `stuck` as two nouls about the status summary; true at ≥ 0.5, or only above the role's `confidence`. |
| `approve` | today's judge, both for gates and open asks | a **closed** ask — a policy gate (approve or deny), or a `human` step whose answer `schema` is an `enum` or a `boolean` — as a choice over its answers; an **open** question cannot be answered by a decision model, so it goes to the model's `escalate` model, or is `UNDECIDED` as today when there is none. Never approves over untrusted input (§8). |
| `compaction`, `turn`, `think` | as today | refused at load |

### 5.5 The decide contract

One internal type serves every decision point, native or emulated:

```rust
pub struct Decision { pub state: Value, pub questions: BTreeMap<String, Question> }
pub enum Question {
    Choice { instructions: String, options: BTreeMap<String, String> },
    Noul   { instructions: String },
    Score  { instructions: String, levels: Vec<String> },
}
pub struct Answer {
    pub value: AnswerValue,                      // Choice(String) | Noul(f64) | Score(f64)
    pub probabilities: Option<BTreeMap<String, f64>>,
    pub confidence: Option<f64>,
    pub calibrated: bool,                        // false when emulated
}
```

- **Native** (`decision` kind): `intel/decisions.rs` maps it onto the [JEV-OR] body and back. A vendor limit (questions, options, levels, state size) is the vendor's 4xx, surfaced as the call's error with the vendor's reason — agentd never truncates a state silently.
- **Emulated** (`chat`, `reasoning`): one structured-output call with every question in one schema — `{<id>: {"choice": <enum>, "confidence": <0..1>}}`, `{"noul": <0..1>}`, `{"score": <number>, "confidence": <0..1>}` — through the dialect's existing structured-output path (`intelligence.structured_output`). `probabilities` from logprobs where the dialect returns them (OpenAI-compatible `logprobs` on the choice token); otherwise absent. `calibrated: false` on every emulated answer, so a threshold on it is visibly a threshold on a self-report.
- **Usage** is counted like any call: input and output tokens into the budget and the governor's reservation (§9).

### 5.6 Confidence, abstention, escalation

A decision model's value is that it knows when it does not know. Every use site that decides may set:

- `confidence: <0..1>` — the least confidence an answer is acted on with (for a `noul`, the least distance from 0.5, doubled: `|p − 0.5| × 2`);
- `on_unsure: escalate | fail | continue` — what an answer under it does. `escalate` re-asks the *same* decision on the model's `escalate` model and takes its answer as final, marked `escalated: true`; `fail` fails the node; `continue` hands the answer on marked `unsure: true` for the workflow to branch on. The default is `escalate` when the model names an `escalate` model, else `fail`: a threshold the author set is never quietly ignored. Without `confidence`, nothing is unsure.

Escalation is one hop: the escalate model's answer is final whatever its confidence, so a chain cannot loop. `fallback` stays what it is — unavailability and budget, not doubt.

The roles' answers under their thresholds: preflight → `task`, as a missing verdict is treated today; goal → not achieved, not stuck; approve → `UNDECIDED`, the ask stays with the human.

### 5.7 The reasoning kind

`reasoning: {effort, budget}` on the model; `effort:` on a `think` or `agent` node and on the `think` tool overrides it for that call. Per dialect:

| Dialect | Sent | Also |
|---|---|---|
| `openai` | `reasoning_effort` | no `temperature`; `max_completion_tokens` (already sent); reasoning tokens counted from `completion_tokens_details.reasoning_tokens` |
| `anthropic` | `thinking: {type: enabled, budget_tokens}` (`budget`, or a per-effort default) | no `temperature`; `max_tokens` above the budget; the signed `thinking` blocks of an assistant message are kept and sent back unchanged while the same turn is still calling tools, and dropped once it ends |
| `bedrock` | the model family's `additionalModelRequestFields` | as the family's native dialect |

Keeping thinking blocks across tool rounds needs one change to the transcript: an assistant message may carry opaque reasoning parts, which the dialect that produced them re-sends and every other path ignores. They are never shown on the feed, never logged unless `log_content` is on, and never survive compaction.

## 6. Workflows

**`model:` on any intelligence node** names a model; the node's contract decides which kinds it accepts, checked at load (`config/settings/mod.rs:7503` grows from "is it a declared tier" to "is it a declared model of a kind this node can use"):

| Node | Default role | Kinds |
|---|---|---|
| `agent`, `extract`, `summarize` | `turn` | chat, reasoning |
| `think` | `think` | chat, reasoning |
| `classify`, `route`, `judge`, `decide` | `decide` | any |

**`classify`, `route`, `judge` keep their outputs**, and gain the decision's fields. On a decision model they are one native question; on a chat model, the preset they are today plus `confidence`.

- `classify {input, classes}` → a choice; `classes` may now map each class to a description (`{billing: "payment problems"}`), which a decision model uses as the option's criterion. Output `{class, confidence, probabilities?}`.
- `route {input, choices}` → a choice; output `{choice, confidence, probabilities?}`.
- `judge {input, rubric}` → a noul ("the input satisfies the rubric"), or, when `levels:` is given, a score; output `{verdict: pass|fail, score, probability?, confidence}`. `reasons` is present only from a chat model — a decision model writes no prose. A workflow that reads `reasons` must tolerate its absence; the change is named in the CHANGELOG.

All three take `confidence:` and `on_unsure:` (§5.6).

**A new node, `decide`**, is the native shape — several typed questions about one state, one call:

```yaml
triage:
  kind: decide
  model: jev                                   # default: roles.decide
  state: "{{ inputs.ticket }}"
  questions:
    queue:  { choice: { billing: "payment problems", tech: "bugs and outages", sales: "buying more" } }
    urgent: { noul: "The customer cannot work at all." }
    mood:   { score: [calm, annoyed, angry, furious] }
    refund: { noul: "The customer asks for money back.", instructions: "Only an explicit request counts." }
  confidence: 0.75
  on_unsure: escalate
route_it:
  kind: switch
  depends_on: [triage]
  on: "{{ steps.triage.output.queue.choice }}"
  cases: { billing: …, tech: …, sales: … }
```

Output: one entry per question — `{choice, probabilities, confidence}`, `{noul}`, `{score, probabilities, confidence}` — plus `model`, `escalated`, and `unsure` per question under `on_unsure: continue`. Questions are at most 16 per node and options or levels at most 32 per question (a load-time bound; vendors' own limits are theirs to enforce).

**`effort:`** on `think` and `agent` nodes: `low | medium | high`, refused at load unless the node's model is `reasoning`.

**Documents** may use all of this: a workflow in an instruction names models the operator declared; the boundary is §5.3's.

## 7. In the loop

The agent's own turn keeps one model, `roles.turn`. It reaches the other kinds through tools, so the model decides when it needs them and the operator decides what they are:

- **`think`** (exists, `runtime/tools.rs:1259`) runs on `roles.think`, with an optional `effort` argument. Bound to a reasoning model, it is System Two on demand: a fast turn model hands a hard sub-problem to a slow, deliberate one, without the whole conversation paying for it.
- **`decide`** (new, root and workflow `agent` nodes, granted like any internal tool): `{state, questions, confidence?}` → the answers, on `roles.decide`. When the agent needs a calibrated yes/no or a ranking it can act on — not prose — it asks for one.

Both are ordinary internal tools: listed in the catalogue, narrowed by `agent.tools`, budgeted, audited.

Two loop-level uses of a decision model are left for after measurement (§11 Q3): routing each turn to a model by a preflight decision about its difficulty, and decision-model guards on steps and event matches (`when: {noul: …, confidence: …}`).

## 8. Security

- **Providers are destinations.** A provider receives what the agent sends it — the state of a decision is often a whole ticket or record. Providers are operator-only, egress-checked and credentialed exactly as `intelligence.endpoints` is; a document cannot add one or point a model at one (§5.2, §5.3).
- **Decision models are injectable** [CP]: text inside the state can flip the answer. So a decision is never more trusted than its state. **The `approve` role never approves a call whose arguments, or whose run, carry `untrusted_input`** — whatever the model and its confidence; such an ask stays with the human. (This holds for today's chat judge too, and is checked there in the same change.) Preflight's `risk` never lowers a guard: it can only add one.
- **`approve` needs a threshold on a decision model**: `roles.approve` bound to a `decision` model without a `confidence` is refused at load, so an approval is never acted on at the model's argmax alone.
- **Logs.** A decision's state is content: logged only under `log_content`. Its answers, probabilities and confidence are not content and are always logged (`intel.decision`), so an operator can audit what was decided and how surely.

## 9. Budget, telemetry, resilience

- **Budget.** A decision call reserves and settles like any call; decision vendors bill input only, but the budget counts tokens, not money, so nothing changes in its arithmetic. `budget.degrade.model` must be `chat | reasoning`.
- **Breakers and failover** are per provider (each has its own endpoint list); a model's `fallback` walks across providers.
- **Telemetry.** Every call is labelled with `model`, `kind`, `role` (or node kind): `intel.calls`, `intel.tokens`, latency histograms; `intel.decision` lines with question ids, answers, confidence, `calibrated`, `escalated`; the feed's counters gain decisions and escalations.
- **Mocks.** The built-in mock gains a decision endpoint (`mock:decide:<script>` and a playbook form), so every decision path has an offline end-to-end test, as `mock:` does for chat today.

## 10. What else changes

| Area | Change |
|---|---|
| `config/settings/mod.rs` | `Intelligence.providers`, `.roles`; `ModelTier` → `ModelEntry {kind, provider, model, window, fallback, reasoning, escalate}`; resolution `resolve(use_site, role) -> Resolved {provider, wire_model, kind, reasoning}` replaces `wire_model`/`default_reference` at every call site; validation: kinds per role and per node, chains, providers exist, egress per provider, auth per provider, the document check (§5.3), `approve` operator-only, `turn`/`model` said once |
| `schema.rs`, flags, env | the new paths; generated flags (`--intelligence.roles.decide jev`); `MODEL` stays `intelligence.model`; the docs flag table regenerated |
| classification | `intelligence.providers` and `.roles.approve` operator-only; `.roles` (other keys) document-writable; all reloadable; `every_config_path_is_classified` covers them |
| `intel/` | `decisions.rs` (new dialect); `openai.rs`, `anthropic.rs`, `bedrock.rs`: reasoning parameters, no temperature, reasoning parts; one `IntelClient` per provider |
| `runtime/reactor.rs` | the single `intel_uri`/headers/bearer/signer/dialect become a per-provider map; `IntelConfig` for a turn worker is built from the resolved provider |
| `runtime/turns.rs` | turn, compaction and preflight through roles; the decision-model preflight split (§5.4) |
| `runtime/goal.rs`, `runtime/human.rs` | the goal and approve roles, native or emulated decisions; the untrusted-input rule |
| `runtime/waits.rs`, `engine/model.rs` | `classify`/`route`/`judge` native path; the `decide` node; `confidence`, `on_unsure`, `effort` fields |
| `runtime/tools.rs`, `registry/internal.rs` | `think` on `roles.think` with `effort`; the `decide` tool |
| `runtime/subagents.rs`, `runtime/instances.rs` | kind checks on spawn models; an instance child's composed `intelligence` carries `providers` (secret references only, as the token is today), `models` and `roles` |
| `context` | opaque reasoning parts on assistant messages (§5.7) |
| `obs/` | labels and the `intel.decision` line |
| docs | `docs/configuration.md` (intelligence section rewritten around providers, models, roles), `docs/workflows.md` (the `decide` node, `confidence`, `effort`), `docs/security.md` (§8), a worked example (`examples/decisions/`), the agentd skill |

## 11. Phases and open questions

**P1 — providers, kinds, roles.** Config, resolution, per-provider clients, roles wired into every call site, the three replaced keys removed, kind checks. Only `chat` behaviour; nothing changes for an unchanged config.
**P2 — decide.** The contract, `dialect: decisions`, emulation, `classify`/`route`/`judge` native, the `decide` node and tool, `confidence`/`on_unsure`/`escalate`, the decision mock.
**P3 — decisions in the loop.** The preflight, goal and approve roles on decision models; §8's untrusted-input rule for every approver.
**P4 — reasoning.** `effort`, the per-dialect mapping, reasoning parts across tool rounds, `think` on `roles.think`.

Decided by the user, 2026-10-10:

- **Q1 — May a decision model approve on the operator's behalf?** Yes, guarded: only bound explicitly, operator-only, with a required threshold, and never over untrusted input (§8).
- **Q2 — Remove the three old keys outright?** Yes, per the standing no-legacy rule; `--model`/`MODEL` are unaffected.
- **Q3 — Per-turn model routing and decision guards (§7)?** After P4, once decision calls are measured in real runs.
- **Scope:** all four phases, in order, each reviewed once, gated and pushed when green.

Open, with recommendations:
- **Q4 — Embeddings as a fourth kind?** Recommendation: not until something in agentd consumes vectors (knowledge, memory recall).
- **Q5 — OpenAI's Decisions API** has no published request schema yet [OAI-DEC]. Recommendation: build `dialect: decisions` on the shape Jev, OpenRouter and Strands share, and verify OpenAI's when it is published.
