// SPDX-License-Identifier: AGPL-3.0-only
//! **The taint engine** (RFC 0044 §5.13 item 5, T11, T12): which work has
//! read outside text, and which holds an enrolled person's delegated data —
//! the facts the person's consent rules (`runtime::delegated`) and the
//! policy matchers `tainted` and `holds_delegated_data` read.
//!
//! It is the outside-input record runs, children and conversations already
//! keep ([`Carries`]), carried the same ways: what a context starts,
//! spawns, signals, steers or sends a message to carries what it carried, a
//! result read back whole hands its record to the reader, an event a run
//! emits carries the run's, and every record is durable. Read for taint,
//! the record says:
//!
//! - **tainted** — it holds an origin of outside text (a webhook's or an
//!   A2A start's route, the resources a `subscribe` start reads from an
//!   `untrusted_input` server, an A2A peer agent's text, a Company's
//!   content, an untrusted delegated result, a run or child no longer on
//!   record) or a taint source the withholding does not follow: an
//!   `untrusted_input` tool's result, a non-operator principal's A2A
//!   message, an A2A peer's answer to a step, an `http` step's response, an
//!   `untrusted_input` server's `mcp.tool` or `mcp.resource` result or the
//!   resource a `wait` or a `subscribe` start reads from it, knowledge
//!   context, a server's skill, a memory entry tainted work wrote. A server
//!   that declares no tag at all — no leg, not `trusted` — is an
//!   `untrusted_input` one: what it answers taints as theirs does. So is
//!   one an instruction document declares, whatever legs it names, unless
//!   the operator's catalog vouches for it
//!   (`config::settings::untrust_document_servers`). A deployment whose
//!   instruction is served (a URL, an OCI artifact, an MCP resource) has
//!   every unit tainted: every turn's prompt holds that text.
//! - **holds delegated data** — a delegated server's result reached it,
//!   trusted (`auth.trusted_output`) or not.
//!
//! Each source is recorded **before** its text is read: a worker announces
//! a call before it makes it (`turn.tool`, a subagent's `tool.called`), a
//! delegated or Company result's record is sent before the result, a step's
//! result is recorded as the step finishes, before a step after it runs.
//!
//! **Not followed**, said: text inside an instance child (a daemon with its
//! own record; what it answers its parent is a peer's answer), and an MCP
//! elicitation's answer, which reaches only the server that asked, never a
//! model — the server's result after it is judged as that server's.

use super::reactor::{Runtime, Target};
use super::tools::ToolCaller;
use super::withhold::Reader;
use crate::context::skills::SkillId;
use crate::engine::run::Carries;
use crate::sec::scope::TrifectaTag;
use crate::supervisor::tree::NodeId;
use serde_json::{Map, Value, json};

/// What the consent rules and the policy matchers know of one unit.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Facts {
    pub tainted: bool,
    pub holds_delegated_data: bool,
    /// Where the outside text it read entered, for a gate to show.
    pub sources: Vec<String>,
}

/// What a person's own message is recorded as when the session it came
/// through proves them the holder of their grant: the session's principal
/// is the grant's, and — for a device name — the session itself signed in
/// as the identity the name is bound to (the user's decision of
/// 2026-10-21). Outside text to every unit but work that acts with that
/// person's grant ([`Facts::of`]).
const OWN_MESSAGE: &str = "a message of the signed-in grant holder ";

/// [`OWN_MESSAGE`] for principal `id`.
pub(crate) fn own_message(id: &str) -> String {
    format!("{OWN_MESSAGE}{id}")
}

impl Facts {
    /// The facts of a unit whose record is `c`, in a deployment whose
    /// instruction is `served` from outside, when it is, for work that acts
    /// with `own`'s grant, when it does: `own`'s own messages are not
    /// outside text to it.
    pub(crate) fn of(c: &Carries, served: Option<&str>, own: Option<&str>) -> Facts {
        let mut sources = c.taint_sources();
        if let Some(own) = own {
            let mine = own_message(own);
            sources.retain(|s| *s != mine);
        }
        if let Some(s) = served {
            sources.push(format!("the served instruction {s}"));
        }
        Facts {
            tainted: !sources.is_empty(),
            holds_delegated_data: c.holds_delegated_data(),
            sources,
        }
    }

    /// The facts as a record shows them.
    pub(crate) fn to_value(&self) -> Value {
        json!({"tainted": self.tainted, "holds_delegated_data": self.holds_delegated_data,
               "taint": self.sources})
    }
}

/// The taint source of an `untrusted_input` tool's result.
fn tool_source(tool: &str) -> String {
    format!("the untrusted_input tool {tool}")
}

impl Runtime {
    /// Where the agent's instruction is served from, when it is not the
    /// operator's own text, file or folder: every turn's prompt holds it.
    pub(crate) fn served_instruction(&self) -> Option<String> {
        let spec = &self.settings.agent.instruction_spec;
        if let Some(u) = &spec.url {
            return Some(u.clone());
        }
        if let Some(o) = &spec.oci {
            return Some(format!("oci://{}", o.reference()));
        }
        if let Some(m) = &spec.mcp {
            return Some(m.to_uri());
        }
        self.settings
            .agent
            .instruction_is_uri()
            .then(|| self.settings.agent.instruction.clone().unwrap_or_default())
    }

    /// The facts of the unit behind `caller`: a firing's work, which
    /// nobody reads as it runs, is tainted whatever it read (RFC 0044 §5.12
    /// E), so the person's consent rules ask them — and deny unanswered.
    pub(crate) fn caller_facts(&self, caller: &ToolCaller) -> Facts {
        let own = self
            .delegated_work(caller)
            .then_some(caller.principal.as_deref())
            .flatten();
        let mut f = Facts::of(
            &self.caller_carries(caller),
            self.served_instruction().as_deref(),
            own,
        );
        if self.firing_caller(caller) {
            f.sources
                .push("nobody there: a firing acting for its person with nobody there".into());
            f.tainted = true;
        }
        f
    }

    /// Whether `caller`'s work is a firing of an `act_for` workflow's.
    fn firing_caller(&self, caller: &ToolCaller) -> bool {
        caller
            .session
            .as_deref()
            .is_some_and(|s| s.starts_with(crate::engine::run::UNATTENDED_SID_PREFIX))
    }

    /// The facts of run `run`.
    pub(crate) fn run_facts(&self, run: &str) -> Facts {
        self.caller_facts(&self.run_caller(run))
    }

    /// Who run `run`'s own work is, as a call it makes is.
    pub(crate) fn run_caller(&self, run: &str) -> ToolCaller {
        let r = self.runs.get(run);
        ToolCaller {
            run: Some(run.to_string()),
            principal: r.and_then(|r| r.principal.clone()),
            ctx: r.and_then(|r| r.conversation.clone()),
            session: r.and_then(|r| r.session.clone()),
            ..ToolCaller::default()
        }
    }

    /// Whether `caller`'s work acts with an enrolled person's grant (RFC
    /// 0044 §5.3, the `mode` fact): the person it is for holds one usable
    /// through the session it acts through — and a subagent's work, or a
    /// run one started, only where their template says `subagents:
    /// inherit`.
    pub(crate) fn delegated_work(&self, caller: &ToolCaller) -> bool {
        #[cfg(feature = "xaa")]
        {
            self.work_templates(caller).is_some()
        }
        #[cfg(not(feature = "xaa"))]
        {
            let _ = caller;
            false
        }
    }

    /// Whether a message principal `id` sent through `session` is their
    /// own as the holder of their grant: work for them acting through that
    /// session would act with it — the session's proven principal is the
    /// grant's — a per-person operator's over the unix listener as much as a
    /// display session's. Never the bare operator's, who holds no grant.
    pub(crate) fn message_is_own(&self, id: &str, session: Option<&str>) -> bool {
        #[cfg(feature = "xaa")]
        {
            self.delegation.as_ref().is_some_and(|d| {
                d.acting_templates(crate::delegation::Acting::new(Some(id), session))
                    .is_some()
            })
        }
        #[cfg(not(feature = "xaa"))]
        {
            let _ = (id, session);
            false
        }
    }

    /// Everything a rule may know of `caller`'s call of `name` beside its
    /// tool and tags (RFC 0044 §5.13 item 6): whether it is made with a
    /// person's grant, what the work read and holds, whether anyone is
    /// there, whether a document's workflow makes it, and whose authority
    /// the work acts with.
    pub(crate) fn policy_facts(
        &self,
        caller: &ToolCaller,
        name: &str,
    ) -> crate::sec::policy::Facts {
        let f = self.caller_facts(caller);
        crate::sec::policy::Facts {
            delegated: self.delegated_tool(name),
            tainted: f.tainted,
            holds_delegated_data: f.holds_delegated_data,
            unattended: self.unattended(caller),
            document_sourced: self.document_sourced(caller, self.delegated_server_name(name)),
            delegated_work: self.delegated_work(caller),
        }
    }

    /// Whether `caller`'s work has nobody there to answer: a trigger started
    /// it, and it runs as `identity.autonomous_as` or as nobody — or it is a
    /// firing acting for its person with nobody there (RFC 0044 M4).
    pub(crate) fn unattended(&self, caller: &ToolCaller) -> bool {
        self.firing_caller(caller)
            || caller
                .principal
                .as_deref()
                .is_none_or(|p| p == self.settings.identity.autonomous_id())
    }

    /// The run `caller`'s work is part of: its own, or for a subagent's the
    /// run that spawned it, through every subagent between as their
    /// `requested_by` records them; `None` for work of no run.
    pub(crate) fn originating_run(
        &self,
        caller: &ToolCaller,
    ) -> Option<&crate::engine::run::RunState> {
        if let Some(r) = caller.run.as_deref() {
            return self.runs.get(r);
        }
        let mut handle = caller.subagent.clone()?;
        // Each hop is a distinct record: the bound only guards a cycle.
        for _ in 0..=self.subagents.len() {
            let by = self.subagents.get(&handle)?.requested_by.as_ref()?;
            if let Some(r) = by["run"].as_str() {
                return self.runs.get(r);
            }
            handle = by["subagent"].as_str()?.to_string();
        }
        None
    }

    /// Whether `caller`'s call is made in a run of a workflow an instruction
    /// document defined — by the run's own step or turn, or by a subagent
    /// one of them spawned — and, for a call to the delegated server
    /// `server`, one the person's template does not list there by name and
    /// digest (`document_workflows`, RFC 0044 §5.12 I).
    pub(crate) fn document_sourced(&self, caller: &ToolCaller, server: Option<String>) -> bool {
        let Some(run) = self.originating_run(caller) else {
            return false;
        };
        if !self
            .definition_for_run_ref(&run.workflow, &run.workflow_hash)
            .is_some_and(|w| w.document)
        {
            return false;
        }
        #[cfg(feature = "xaa")]
        if let (Some(server), Some(d)) = (server, &self.delegation)
            && let Ok(a) = self.delegation_admit(d, caller, &server)
            && a.template.servers.get(&server).is_some_and(|s| {
                s.document_workflows
                    .iter()
                    .any(|w| w.name == run.workflow && w.digest == run.workflow_hash)
            })
        {
            return false;
        }
        #[cfg(not(feature = "xaa"))]
        let _ = server;
        true
    }

    /// The delegated server whose tool `name` is, when it is one.
    pub(crate) fn delegated_server_name(&self, name: &str) -> Option<String> {
        match self.registry.route(name)? {
            crate::registry::Route::Mcp { server, .. } if self.delegated(server) => {
                Some(server.to_string())
            }
            crate::registry::Route::Mapped(m) if self.delegated(&m.server) => {
                Some(m.server.clone())
            }
            _ => None,
        }
    }

    /// Add `carries` to the record of the context `reader` is.
    pub(crate) fn hand_reader(&mut self, reader: &Reader, carries: &Carries) {
        match reader {
            Reader::Run(id) => self.hand_run(id, carries),
            Reader::Child(h) => self.steer_carries(h, carries),
            Reader::Conversation(Some(ctx)) => self.hand_conversation(ctx, carries),
            Reader::Conversation(None) => {}
        }
    }

    /// Add `carries` to the record of the context `caller`'s reply is read
    /// into — a memory entry it read. A call no model makes reads nothing.
    pub(crate) fn hand_caller(&mut self, caller: &ToolCaller, carries: &Carries) {
        if carries.is_empty() {
            return;
        }
        if let Some(r) = self.reader_of(caller) {
            self.hand_reader(&r, carries);
        }
    }

    /// The taint source of the registry's tool `name`'s result, when it is
    /// outside text: the tool is tagged `untrusted_input`, or a server that
    /// declares no tag serves it ([`Self::untagged_server`]).
    fn tool_taint(&self, name: &str) -> Option<String> {
        if self
            .registry
            .tags_of(std::slice::from_ref(&name.to_string()))
            .contains(&TrifectaTag::UntrustedInput)
        {
            return Some(tool_source(name));
        }
        let server = self.registry.get(name)?.server.as_deref()?;
        self.untagged_server(server)
            .then(|| format!("the tool {name} of the untagged server {server}"))
    }

    /// The child `node` announced a call to `tool`: an `untrusted_input`
    /// tool's result, or an untagged server's, taints the unit it is read
    /// into — recorded now, before the call is made, so before its result is
    /// read.
    pub(crate) fn taint_announced(&mut self, node: NodeId, tool: &str) {
        let Some(source) = self.tool_taint(tool) else {
            return;
        };
        let reader = self.reader_at(&Target::Child(node, 0));
        let mut c = Carries::default();
        c.taint([source]);
        self.hand_reader(&reader, &c);
    }

    /// Whether a worker's call to a tool tagged `tags` comes back to the
    /// supervisor rather than being dialled by the worker: where a person's
    /// grant is delegated, every `egress` tool does, since from work that is
    /// tainted and holds their data it asks the person (RFC 0044 §5.13 item
    /// 6) — a call the supervisor never sees is one no rule can hold.
    pub(crate) fn egress_round_trips(&self, tags: &[TrifectaTag]) -> bool {
        #[cfg(feature = "xaa")]
        {
            self.delegation.is_some() && tags.contains(&TrifectaTag::Egress)
        }
        #[cfg(not(feature = "xaa"))]
        {
            let _ = tags;
            false
        }
    }

    /// Whether the registry's tool `name` is served by a delegated server:
    /// a call to it is made with an enrolled person's grant.
    pub(crate) fn delegated_tool(&self, name: &str) -> bool {
        match self.registry.route(name) {
            Some(crate::registry::Route::Mcp { server, .. }) => self.delegated(server),
            Some(crate::registry::Route::Mapped(m)) => self.delegated(&m.server),
            _ => false,
        }
    }

    /// Whether `server` is a delegated server, whose results the call that
    /// made it records.
    fn delegated(&self, server: &str) -> bool {
        #[cfg(feature = "xaa")]
        {
            self.is_delegated_server(server)
        }
        #[cfg(not(feature = "xaa"))]
        {
            let _ = server;
            false
        }
    }

    /// Whether `server` is one the configuration declares with no tag at all
    /// — no leg, not `trusted` — whose results count as `untrusted_input`
    /// (`McpServer::counted_tags`). None of agentd's own is: `code`, the tools
    /// an embedder registers in its own program, is declared nowhere (the name
    /// is reserved); a delegated server must carry a leg (`id_jag_untagged`),
    /// and the call that made it records its result; a Company's servers take
    /// its entry's tags, or `untrusted_input` and `egress` when it declares
    /// none, and what they answer is the Company's origin besides.
    fn untagged_server(&self, server: &str) -> bool {
        !self.delegated(server)
            && self
                .settings
                .mcp
                .servers
                .iter()
                .any(|s| s.name == server && s.untagged())
    }

    /// The taint source of a result of `server`, when it is outside text: its
    /// declared `tags` name `untrusted_input`, for any tool or for every one,
    /// or a registry tool it serves carries it — so a server that serves only
    /// resources is judged by what it declares — or it declares no tag at all
    /// ([`Self::untagged_server`]).
    fn server_taint(&self, server: &str) -> Option<String> {
        let declared = self
            .settings
            .mcp
            .servers
            .iter()
            .find(|s| s.name == server)
            .and_then(|s| s.tag_set().ok())
            .is_some_and(|t| t.contains(&TrifectaTag::UntrustedInput));
        if declared
            || self.registry.iter().any(|t| {
                t.server.as_deref() == Some(server) && t.tags.contains(&TrifectaTag::UntrustedInput)
            })
        {
            return Some(format!("a result of the untrusted_input server {server}"));
        }
        self.untagged_server(server)
            .then(|| format!("a result of the untagged server {server}"))
    }

    /// What a finished step of `kind`, defined with `spec`, hands its run of
    /// outside text, if anything: an `http` step's response, an A2A peer's
    /// answer, an `untrusted_input` tool's result, or an untagged server's,
    /// through a `tool` step, a `memory.*` or `artifact.*` step a
    /// `tools.overrides` entry maps onto a server, or a built-in `search.*`
    /// step, a `knowledge.*` step's result (as knowledge context taints a
    /// turn), and an `mcp.tool` or `mcp.resource` result, or the resource a
    /// `wait {on: resource}` reads, from an `untrusted_input` or untagged
    /// server ([`Self::server_taint`]) — the one the attempt named, rendered
    /// (`server`), or, for a step that ended before it was dispatched, the
    /// one its definition names. A delegated server's result is recorded by
    /// the call that made it.
    pub(crate) fn taint_step(
        &mut self,
        run_id: &str,
        kind: &str,
        spec: &Map<String, Value>,
        server: Option<&str>,
    ) {
        let server = server.or_else(|| spec.get("server").and_then(Value::as_str));
        let source = match kind {
            "http" => Some(format!(
                "the response of an http step to {}",
                spec.get("url").and_then(Value::as_str).unwrap_or("a URL")
            )),
            "a2a.send" | "a2a.delegate" | "a2a.wait" => Some(format!(
                "the A2A peer {}'s answer",
                spec.get("to")
                    .or_else(|| spec.get("peer"))
                    .and_then(Value::as_str)
                    .unwrap_or("of a step")
            )),
            "tool" => spec
                .get("name")
                .and_then(Value::as_str)
                .and_then(|t| self.tool_taint(t)),
            k if k.starts_with("knowledge.") => Some(format!(
                "knowledge context from {}",
                self.settings
                    .knowledge
                    .server
                    .as_deref()
                    .unwrap_or("the knowledge server")
            )),
            // A contract agentd implements itself serves no server and
            // carries no `untrusted_input`: only one a server implements
            // taints.
            k if k.starts_with("search.")
                || k.starts_with("memory.")
                || k.starts_with("artifact.") =>
            {
                self.tool_taint(k)
            }
            "mcp.tool" | "mcp.resource" => server
                .filter(|s| !self.delegated(s))
                .and_then(|s| self.server_taint(s)),
            // What a resource wait resolves with is the resource, read.
            "wait" if spec.get("on").and_then(Value::as_str) == Some("resource") => {
                server.and_then(|s| self.server_taint(s))
            }
            _ => None,
        };
        if let Some(s) = source {
            let mut c = Carries::default();
            c.taint([s]);
            self.hand_run(run_id, &c);
        }
    }

    /// What the run a `subscribe` start fires carries of the resource it
    /// fires with: that read is `server`'s answer, so an `untrusted_input`
    /// or untagged server's taints the run from its start
    /// ([`Self::server_taint`]).
    pub(crate) fn subscribe_carries(&self, server: &str) -> Carries {
        let mut c = Carries::default();
        if let Some(s) = self.server_taint(server) {
            c.taint([s]);
        }
        c
    }

    /// The skill `id`, read as `reference`, reached `reader`: one an MCP
    /// server serves is that server's text whatever the server is tagged
    /// (RFC 0051 D4) — recorded before its body is handed over. A skill of
    /// the operator's own, from a folder or the instruction, is
    /// configuration.
    pub(crate) fn taint_skill(&mut self, reader: &Reader, id: &SkillId, reference: &str) {
        let SkillId::Server { server, .. } = id else {
            return;
        };
        let mut c = Carries::default();
        c.taint([format!("the skill {reference} of the MCP server {server}")]);
        self.hand_reader(reader, &c);
    }

    /// Knowledge context from `server` reached conversation `ctx`'s turn.
    pub(crate) fn taint_knowledge(&mut self, ctx: &str) {
        let server = self
            .settings
            .knowledge
            .server
            .clone()
            .unwrap_or_else(|| "the knowledge server".into());
        let mut c = Carries::default();
        c.taint([format!("knowledge context from {server}")]);
        self.hand_conversation(ctx, &c);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A record's facts: any origin but a delegated one, or any taint source,
    /// taints it; a delegated origin alone holds data and does not; a served
    /// instruction taints everything.
    #[test]
    fn the_facts_follow_the_record() {
        let mut trusted = Carries::default();
        trusted.add(["delegated:linear".to_string()], || "read".into());
        let f = Facts::of(&trusted, None, None);
        assert!(f.holds_delegated_data && !f.tainted, "{f:?}");
        let mut untrusted = trusted.clone();
        untrusted.taint(["delegated:linear".to_string()]);
        assert!(Facts::of(&untrusted, None, None).tainted);
        let mut peer = Carries::default();
        peer.add(["the A2A peer agent:x".to_string()], || "peer".into());
        let f = Facts::of(&peer, None, None);
        assert!(f.tainted && !f.holds_delegated_data, "{f:?}");
        assert_eq!(f.sources, ["the A2A peer agent:x"]);
        let f = Facts::of(&Carries::default(), Some("https://x.example/a.md"), None);
        assert!(
            f.tainted && f.sources[0].contains("served instruction"),
            "{f:?}"
        );
        assert_eq!(Facts::of(&Carries::default(), None, None), Facts::default());
    }

    /// A person's own message, sent through a session that proves them the
    /// holder of their grant, is not outside text to work acting with that
    /// grant — and is to work for anyone else, and to work acting with no
    /// grant at all; a tool's result beside it still taints their own work.
    /// A per-person operator holding a grant is held the same way.
    #[test]
    fn a_persons_own_message_is_outside_text_to_anyone_else() {
        let alice = "user:oidc:https://idp.example#alice";
        let mut c = Carries::default();
        c.taint([own_message(alice)]);
        assert!(!Facts::of(&c, None, Some(alice)).tainted, "her own work");
        assert!(Facts::of(&c, None, Some("user:oidc:https://idp.example#bob")).tainted);
        assert!(
            Facts::of(&c, None, None).tainted,
            "work acting with no grant"
        );
        c.taint([tool_source("web.fetch")]);
        let f = Facts::of(&c, None, Some(alice));
        assert!(
            f.tainted && f.sources == [tool_source("web.fetch")],
            "{f:?}"
        );
        // A per-person operator holding a grant is a person too: their own
        // message is theirs alone, never trusted text to another's grant.
        use crate::config::settings::Role;
        use crate::runtime::withhold::principal_carries;
        let op = "operator:carol";
        let theirs = principal_carries(op, Some(Role::Operator), true, "a message");
        assert!(
            !Facts::of(&theirs, None, Some(op)).tainted,
            "their own work"
        );
        assert!(
            Facts::of(&theirs, None, Some(alice)).tainted,
            "another's work"
        );
        // An operator who holds no grant writes nothing outside.
        let bare = principal_carries("operator", Some(Role::Operator), false, "a message");
        assert!(!Facts::of(&bare, None, Some(alice)).tainted);
        // A person who does not prove the holder's session is anyone.
        let anyone = principal_carries(alice, Some(Role::User), false, "a message");
        assert!(Facts::of(&anyone, None, Some(alice)).tainted);
    }

    /// What a conversation or a memory entry keeps of a record: delegated
    /// origins, Companies only for a conversation, every other origin as
    /// taint — so it stays tainted without being an origin a reader is
    /// withheld from.
    #[test]
    fn a_holder_keeps_the_data_and_the_taint_and_no_other_origin() {
        let mut c = Carries::default();
        c.add(
            [
                "delegated:linear".to_string(),
                "company:https://a.example".to_string(),
                "the route of workflow \"hook\"".to_string(),
            ],
            || "why".into(),
        );
        c.taint(["the untrusted_input tool web.fetch".to_string()]);
        let conv = c.kept_by_holder(true);
        assert!(conv.origins.contains("delegated:linear"));
        assert!(conv.origins.contains("company:https://a.example"));
        assert!(!conv.origins.contains("the route of workflow \"hook\""));
        assert!(conv.taint.contains("the route of workflow \"hook\""));
        assert!(conv.taint.contains("the untrusted_input tool web.fetch"));
        let mem = c.kept_by_holder(false);
        assert!(!mem.origins.iter().any(|o| o.starts_with("company:")));
        assert!(mem.tainted() && mem.holds_delegated_data());
        // A union carries the taint too.
        let mut r = Carries::default();
        assert!(r.union(&mem));
        assert!(r.tainted() && !r.union(&mem), "nothing new the second time");
    }
}
