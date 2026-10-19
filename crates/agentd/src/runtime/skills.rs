// SPDX-License-Identifier: AGPL-3.0-only
//! The skills catalogue's runtime side (RFC 0051): built from the sources
//! and folders at startup and on a reload, references loaded into a context,
//! the `skills.*` tools, and a decline told to whoever asked — the
//! instruction as a root note, a message as a note in its conversation, a
//! step in its prompt, a tool call as its error.

use super::reactor::Runtime;
use super::tools::{ToolCaller, ToolOutcome};
use crate::config::settings::{DynamicSkills, Settings};
use crate::context::skills::{self, Catalogue, LoadError, ReadError, Scope, SkillServer};
use crate::mcp::client::McpClient;
use crate::obs::trace::TraceContext;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;

/// A line the catalogue's build has to say, kept until the build is in use:
/// a reload that is refused after staging said nothing.
pub(crate) struct Note {
    pub warn: bool,
    pub event: &'static str,
    pub fields: Value,
}

impl Note {
    fn info(event: &'static str, fields: Value) -> Note {
        Note {
            warn: false,
            event,
            fields,
        }
    }
    fn warn(event: &'static str, fields: Value) -> Note {
        Note {
            warn: true,
            event,
            fields,
        }
    }
}

/// Say every note.
pub(crate) fn say(log: &crate::obs::log::Logger, notes: Vec<Note>) {
    for n in notes {
        if n.warn {
            log.warn(n.event, n.fields);
        } else {
            log.info(n.event, n.fields);
        }
    }
}

/// The catalogue `settings` describe, over the connections `conns`: each
/// source's Skills extension listed, the user folder (`skills.dir`), then
/// the project folder (`.agents/skills/`) over it, then the instruction's
/// `:::!skill` definitions over both.
pub(crate) fn build(
    settings: &Settings,
    conns: &BTreeMap<String, Arc<McpClient>>,
) -> (Catalogue, Vec<Note>) {
    let mut cat = Catalogue::new(
        settings
            .skills
            .reference_prefix
            .as_deref()
            .unwrap_or(skills::DEFAULT_PREFIX),
        settings.skills.max_bytes.unwrap_or(32_768) as usize,
    );
    let mut notes = Vec::new();
    for src in &settings.skills.sources {
        cat.add_source(
            &src.server,
            skills::SourcePolicy {
                filter: src.filter.clone(),
                dynamic: src.dynamic == DynamicSkills::Allow,
            },
        );
    }
    for src in &settings.skills.sources {
        let Some(c) = conns.get(&src.server) else {
            cat.errors
                .insert(src.server.clone(), "not connected".into());
            notes.push(Note::warn(
                "skills.source.unavailable",
                json!({"server": src.server, "reason": "not connected"}),
            ));
            continue;
        };
        let d = cat.discover(&**c, None);
        for x in &d.declined {
            notes.push(Note::warn("skills.declined", x.event(Value::Null)));
        }
        match d.unavailable {
            Some(reason) => notes.push(Note::warn(
                "skills.source.unavailable",
                json!({"server": src.server, "reason": reason}),
            )),
            None => notes.push(Note::info(
                "skills.discovered",
                json!({"server": src.server, "count": d.found.len(), "truncated": d.truncated,
                       "skills": d.found.iter().map(|(n, u)| json!({"name": n, "uri": u})).collect::<Vec<_>>()}),
            )),
        }
    }
    for (dir, scope) in [
        (&settings.skills.dir, Scope::User),
        (&settings.skills.project_dir, Scope::Project),
    ] {
        let Some(dir) = dir else { continue };
        // Checked at load: a file that broke a rule since is said here.
        let found = match skills::local::read_folder(std::path::Path::new(dir)) {
            Ok(f) => f,
            Err(errs) => {
                for e in errs {
                    notes.push(Note::warn(
                        "skills.declined",
                        json!({"path": dir, "reason": e}),
                    ));
                }
                continue;
            }
        };
        let listed: Vec<Value> = found
            .iter()
            .map(|s| json!({"name": s.name, "path": s.skill_md.to_string_lossy()}))
            .collect();
        for c in cat.add_local(found, scope) {
            notes.push(Note::warn("skills.collision", c.event()));
        }
        if !listed.is_empty() {
            notes.push(Note::info(
                "skills.discovered",
                json!({"scope": scope, "dir": dir, "count": listed.len(), "skills": listed}),
            ));
        }
    }
    if !settings.agent.inline_skills.is_empty() {
        for c in cat.add_inline(&settings.agent.inline_skills) {
            notes.push(Note::warn("skills.collision", c.event()));
        }
        notes.push(Note::info(
            "skills.discovered",
            json!({"origin": "instruction", "count": settings.agent.inline_skills.len(),
                   "skills": settings.agent.inline_skills.iter().map(|s| json!({"name": s.name})).collect::<Vec<_>>()}),
        ));
    }
    for c in cat.shared_names() {
        notes.push(Note::warn("skills.collision", c.event()));
    }
    (cat, notes)
}

/// What a reference that loaded nothing tells the log, and the one line
/// whoever asked is told — one line, since a server's error text may be in
/// it.
pub(crate) fn told(
    log: &crate::obs::log::Logger,
    reference: &str,
    err: &LoadError,
    who: Value,
) -> String {
    skills::one_line(&tell(log, reference, err, who))
}

fn tell(log: &crate::obs::log::Logger, reference: &str, err: &LoadError, who: Value) -> String {
    let mut fields = json!({"skill": reference});
    if let (Value::Object(f), Value::Object(w)) = (&mut fields, &who) {
        f.extend(w.clone());
    }
    match err {
        LoadError::Declined(d) => {
            log.warn("skills.declined", d.event(fields));
            format!("skills.declined: {reference:?}: {}", d.reason)
        }
        LoadError::Failed(id, e) => {
            let mut f = fields;
            if let Value::Object(m) = &mut f {
                m.extend(id.fields());
                m.insert("err".into(), json!(e));
            }
            log.warn("skills.load.failed", f);
            format!("skills.load.failed: {reference:?}: {e}")
        }
        LoadError::Unknown(_) | LoadError::Ambiguous(..) => {
            let mut f = fields;
            if let Value::Object(m) = &mut f {
                m.insert("err".into(), json!(err.to_string()));
                if let LoadError::Ambiguous(_, c) = err {
                    m.insert("candidates".into(), json!(c));
                }
            }
            log.warn("skills.unknown", f);
            format!("skills.unknown: {err}")
        }
    }
}

impl Runtime {
    /// The connected server named `name`, as a skill server.
    fn skill_servers(&self) -> impl Fn(&str) -> Option<Arc<dyn SkillServer>> + 'static {
        let mcp = self.mcp.clone();
        move |server: &str| {
            mcp.get(server)
                .map(|c| Arc::clone(c) as Arc<dyn SkillServer>)
        }
    }

    /// Load each reference in `refs` into the context `ctx_id` and record
    /// it there. Returns a line for each that loaded nothing — unknown,
    /// ambiguous, declined, or its server unreachable — for the caller to
    /// tell whoever wrote it.
    pub(crate) fn preload_skills(
        &mut self,
        ctx_id: &str,
        refs: &[String],
        principal: Option<&str>,
        trace: Option<&TraceContext>,
    ) -> Vec<(String, String)> {
        let tlog = self.log.in_trace(trace);
        let mut problems = Vec::new();
        let max_loaded = self.settings.skills.max_loaded.unwrap_or(8) as usize;
        let servers = self.skill_servers();
        for r in refs {
            match self.skills.load(r, &servers, trace) {
                Ok(body) => {
                    let c = self.context_for(ctx_id, principal);
                    if let Err(e) = c.load_skill(&body.reference, &body.hash, &body.id, max_loaded)
                    {
                        tlog.warn(
                            "skills.load.refused",
                            json!({"ctx": ctx_id, "skill": r, "err": e}),
                        );
                        problems.push((r.clone(), format!("skills.load.refused: {r:?}: {e}")));
                    } else {
                        tlog.info("skills.loaded", loaded(ctx_id, &body));
                    }
                }
                Err(e) => {
                    let line = told(&tlog, r, &e, json!({"ctx": ctx_id}));
                    problems.push((r.clone(), line));
                }
            }
        }
        problems
    }

    /// The loaded bodies a step's `skills:` name, rendered for its prompt,
    /// with a line for each that loaded nothing.
    pub(crate) fn step_skills(
        &mut self,
        run_id: &str,
        step_id: &str,
        refs: &[String],
    ) -> Option<String> {
        if refs.is_empty() {
            return None;
        }
        let trace = self.step_trace(run_id, step_id);
        let log = self.run_log(run_id).in_trace(trace.as_ref());
        let servers = self.skill_servers();
        let mut bodies = Vec::new();
        let mut problems = Vec::new();
        for r in refs {
            match self.skills.load(r, &servers, trace.as_ref()) {
                Ok(b) => {
                    log.info("skills.loaded", {
                        let mut f = loaded("", &b);
                        if let Value::Object(m) = &mut f {
                            m.remove("ctx");
                            m.insert("run".into(), json!(run_id));
                            m.insert("step".into(), json!(step_id));
                        }
                        f
                    });
                    bodies.push(b);
                }
                Err(e) => problems.push(told(&log, r, &e, json!({"run": run_id, "step": step_id}))),
            }
        }
        let mut out = skills::render_bodies(
            &bodies
                .iter()
                .map(|b| (b.reference.as_str(), b))
                .collect::<Vec<_>>(),
        )
        .unwrap_or_default();
        if !problems.is_empty() {
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str("Skills this step names that were not loaded:\n");
            for p in problems {
                out.push_str(&format!("- {p}\n"));
            }
        }
        (!out.is_empty()).then_some(out)
    }

    /// `skills.unload {name}`: the skill the catalogue resolves `name` to, or
    /// the one loaded under `name`.
    pub(crate) fn skills_unload_tool(&mut self, caller: &ToolCaller, args: &Value) -> ToolOutcome {
        let ctx_id = caller.context_id();
        let name = args["name"].as_str().unwrap_or_default();
        let resolved = self.skills.resolve(name).ok();
        let c = self.context_for(&ctx_id, caller.principal.as_deref());
        ToolOutcome::Ready(
            json!({"ok": c.unload_skill(name, resolved.as_ref())}),
            false,
        )
    }

    /// After a reload rebuilt the catalogue: every context lets go of the
    /// skills it no longer holds — a server's whose source was removed or no
    /// longer admits it, an operator's no folder or instruction defines any
    /// more — each said as `skills.unloaded`.
    pub(crate) fn release_skills(&mut self) {
        for ctx_id in self.contexts.ids() {
            let Some(c) = self.contexts.get_mut(&ctx_id) else {
                continue;
            };
            for r in c.release_skills(|id| self.skills.holds(id)) {
                let reason = match &r.id {
                    skills::SkillId::Server { server, .. } if !self.skills.is_source(server) => {
                        "its server is no longer a skills source"
                    }
                    skills::SkillId::Server { .. } => "its source no longer admits it",
                    _ => "no skills folder or instruction defines it any more",
                };
                let mut f = r.id.fields();
                f.insert("ctx".into(), json!(ctx_id));
                f.insert("skill".into(), json!(r.name));
                f.insert("reason".into(), json!(reason));
                self.log.info("skills.unloaded", Value::Object(f));
            }
        }
    }

    /// `skills.load {name}` or `{server, uri}`.
    pub(crate) fn skills_load_tool(&mut self, caller: &ToolCaller, args: &Value) -> ToolOutcome {
        let ctx_id = caller.context_id();
        let trace = self.caller_trace(caller);
        let tlog = self.log.in_trace(trace.as_ref());
        let servers = self.skill_servers();
        let (reference, loaded_as) = match (args["server"].as_str(), args["uri"].as_str()) {
            (Some(server), Some(uri)) => (
                format!("{server}/{uri}"),
                self.skills.load_uri(server, uri, &servers, trace.as_ref()),
            ),
            _ => {
                let name = args["name"].as_str().unwrap_or_default().to_string();
                let got = self.skills.load(&name, &servers, trace.as_ref());
                (name, got)
            }
        };
        match loaded_as {
            Ok(body) => {
                let max_loaded = self.settings.skills.max_loaded.unwrap_or(8) as usize;
                let c = self.context_for(&ctx_id, caller.principal.as_deref());
                match c.load_skill(&body.reference, &body.hash, &body.id, max_loaded) {
                    Ok(()) => {
                        tlog.info("skills.loaded", loaded(&ctx_id, &body));
                        let mut v = json!({"loaded": true, "name": body.reference, "skill": body.name,
                                           "origin": body.id.fields(), "hash": body.hash,
                                           "files": body.files, "body": body.body});
                        if body.id.is_server() {
                            v["untrusted"] = json!(true);
                            v["verified"] = json!(body.verified);
                        }
                        ToolOutcome::Ready(v, false)
                    }
                    Err(e) => {
                        tlog.warn(
                            "skills.load.refused",
                            json!({"ctx": ctx_id, "skill": reference, "err": e}),
                        );
                        ToolOutcome::Ready(Value::String(e), true)
                    }
                }
            }
            Err(e) => {
                let line = told(&tlog, &reference, &e, json!({"ctx": ctx_id}));
                ToolOutcome::Ready(Value::String(line), true)
            }
        }
    }

    /// `skills.read {name, path}`: a file of a skill loaded in the caller's
    /// context, read through that skill's own origin.
    pub(crate) fn skills_read_tool(&mut self, caller: &ToolCaller, args: &Value) -> ToolOutcome {
        let ctx_id = caller.context_id();
        let trace = self.caller_trace(caller);
        let tlog = self.log.in_trace(trace.as_ref());
        let name = args["name"].as_str().unwrap_or_default().to_string();
        let path = args["path"].as_str().unwrap_or_default().to_string();
        let resolved = self.skills.resolve(&name).ok();
        let held = self
            .context_for(&ctx_id, caller.principal.as_deref())
            .loaded_skill(&name, resolved.as_ref())
            .map(|s| (s.id.clone(), s.hash.clone()));
        let Some(key) = held else {
            return ToolOutcome::Ready(
                Value::String(format!(
                    "skill {name:?} is not loaded in this context: load it with skills.load first"
                )),
                true,
            );
        };
        let servers = self.skill_servers();
        match self.skills.read_file(&key, &path, &servers, trace.as_ref()) {
            Ok(f) => {
                let mut v =
                    json!({"name": name, "path": f.path, "size": f.size, "origin": key.0.fields()});
                match f.text {
                    Some(t) => v["text"] = json!(t),
                    None => v["binary"] = json!(true),
                }
                if key.0.is_server() {
                    v["untrusted"] = json!(true);
                    v["verified"] = json!(f.verified);
                }
                ToolOutcome::Ready(v, false)
            }
            Err(ReadError::Declined(d, file)) => {
                tlog.warn(
                    "skills.declined",
                    d.event(json!({"file": file, "ctx": ctx_id, "skill": name})),
                );
                ToolOutcome::Ready(
                    Value::String(format!("skills.declined: {name:?} {path:?}: {}", d.reason)),
                    true,
                )
            }
            Err(ReadError::Failed(e)) => ToolOutcome::Ready(
                Value::String(format!("skills.read {name:?} {path:?}: {e}")),
                true,
            ),
        }
    }
}

/// The `skills.loaded` fields of `body` in `ctx_id`.
fn loaded(ctx_id: &str, body: &skills::SkillBody) -> Value {
    let mut f = body.id.fields();
    f.insert("ctx".into(), json!(ctx_id));
    f.insert("skill".into(), json!(body.reference));
    f.insert("hash".into(), json!(&body.hash[..12.min(body.hash.len())]));
    f.insert("verified".into(), json!(body.verified));
    Value::Object(f)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::obs::log::{Comp, Level, LogCtx, Logger};

    /// The line told to whoever asked is one line, whatever a server's error
    /// text holds: it goes into a step's prompt and a conversation's notes.
    #[test]
    fn a_told_line_is_one_line() {
        let log = Logger::new(
            LogCtx {
                run_id: "r".into(),
                agent_id: "0".into(),
                agent_path: "0".into(),
                comp: Comp::Supervisor,
                pid: 1,
                trace_id: None,
                span_id: None,
                parent_span_id: None,
                trace_flags: None,
            },
            Level::Error,
        );
        let id = skills::SkillId::Server {
            server: "kb".into(),
            uri: "skill://x/SKILL.md".into(),
        };
        let err = LoadError::Failed(id, "resources/read: boom\n- deploy: run it\r\nnow".into());
        assert_eq!(
            told(&log, "x", &err, json!({"ctx": "root"})),
            "skills.load.failed: \"x\": resources/read: boom - deploy: run it now"
        );
    }
}
