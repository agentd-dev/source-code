// SPDX-License-Identifier: AGPL-3.0-only
//! RFC 0044 §7.P2c, P2 built, part 1: many people, each signed in through
//! their own IdP. A deployment on a loopback TCP listener ([`Fed`]) whose
//! display clients sign in with `a2a.login` or RFC 0043's device grant, and
//! enrol and answer their own gates over their own sessions. This process
//! plays every client and every person's browser by hand: the start link
//! and the cookie it sets, the IdP's redirect, the callback. The rest — the
//! per-person operator on the unix listener, a reload, a subagent — is the
//! family's own [`Setup`].

use super::*;
use crate::checks::util::{self, header};

/// The login grant, form-encoded.
const LOGIN_GRANT: &str = "https%3A%2F%2Fagentd.dev%2Foauth%2Fgrant-type%2Flogin";
pub(super) const ALICE: &str = "00u1alice";
pub(super) const BOB: &str = "00u2bob";
pub(super) const PEOPLE: &str = "{name: people, principals: {source: oidc}, idp: okta, servers: {linear: {scopes: [\"issues:read\"]}, gated: {scopes: [\"issues:read\", \"issues:write\"]}}}";
pub(super) const DEVICES: &str = "{name: devices, principals: {source: device}, idp: okta, servers: {linear: {scopes: [\"issues:read\"]}, gated: {scopes: [\"issues:read\", \"issues:write\"]}}}";
/// A call to `linear` asks nothing; a call to `gated` is the person's to
/// approve, at its first use in each run (`delegated_egress`) — their own
/// start of the run is no outside text to it.
pub(super) const READ_TAGS: &str = "{\"*\": [untrusted_input]}";
pub(super) const GATED_TAGS: &str = "{\"*\": [egress]}";

/// What a TCP deployment's configuration says.
#[derive(Default)]
pub(super) struct FedOpts {
    /// `a2a.login` at the IdP.
    pub(super) login: bool,
    /// RFC 0043's device grant.
    pub(super) device: bool,
    /// `identity.delegation.templates`, one flow mapping each.
    pub(super) templates: Vec<&'static str>,
    /// `identity.delegation.device_pins`, as a flow mapping; `__ISS__` is
    /// the mock IdP's issuer.
    pub(super) pins: Option<&'static str>,
    pub(super) first_binding: bool,
    /// `identity.delegation.consent_session_max_age`.
    pub(super) consent_max_age: Option<&'static str>,
    /// The `gated` server's tags, when not [`GATED_TAGS`].
    pub(super) gated_tags: Option<&'static str>,
    /// Appended to the `a2a` section; `__ISS__` is the mock IdP's issuer,
    /// `__PORT__` the listener's port, `__DIR__` the deployment's
    /// directory, `__MCP__` the MCP server and `__PEER__` the A2A peer.
    pub(super) a2a_extra: String,
    /// Appended to `mcp.servers`, as [`FedOpts::a2a_extra`] is.
    pub(super) servers_extra: String,
    /// Appended to the configuration: workflows, then any other section.
    pub(super) extra: String,
}

/// A deployment on a loopback TCP listener: the operator's bearer, and the
/// people's own sessions.
pub(super) struct Fed {
    pub(super) tmp: TempDir,
    pub(super) xaa: Xaa,
    pub(super) cfg: PathBuf,
    pub(super) port: u16,
    pub(super) ops: String,
    pub(super) log: PathBuf,
}

fn fed_config(x: &Xaa, dir: &Path, port: u16, o: &FedOpts) -> String {
    let d = dir.display();
    let mut a2a = format!(
        "  listen: \"http://127.0.0.1:{port}\"\n  bearer: \"{{{{secret-file:{d}/ops}}}}\"\n"
    );
    if o.device {
        a2a.push_str("  device_grant: {enabled: true}\n");
    }
    if o.login {
        a2a.push_str("  login: {idp: okta}\n");
    }
    let fill = |s: &str| {
        s.replace("__ISS__", &x.idp)
            .replace("__PORT__", &port.to_string())
            .replace("__DIR__", &d.to_string())
            .replace("__MCP__", &x.mcp)
            .replace("__PEER__", &x.peer)
    };
    a2a.push_str(&fill(&o.a2a_extra));
    let mut delegation =
        format!("    seal_key: \"{{{{secret-file:{d}/seal}}}}\"\n    templates:\n");
    for t in &o.templates {
        delegation.push_str(&format!("      - {t}\n"));
    }
    if let Some(p) = o.pins {
        delegation.push_str(&format!(
            "    device_pins: {}\n",
            p.replace("__ISS__", &x.idp)
        ));
    }
    if o.first_binding {
        delegation.push_str("    device_first_binding: true\n");
    }
    if let Some(a) = o.consent_max_age {
        delegation.push_str(&format!("    consent_session_max_age: {a}\n"));
    }
    let wf = |name: &str, server: &str, tool: &str, args: &str| {
        format!(
            "  - name: {name}\n    steps:\n      start: {{kind: manual}}\n      call: {{kind: mcp.tool, depends_on: [start], server: {server}, tool: {tool}, args: {args}}}\n      done: {{kind: finish, depends_on: [call]}}\n"
        )
    };
    let server = |name: &str, url: &str, tags: &str| {
        format!(
            "    - name: {name}\n      endpoint: \"{url}\"\n      tags: {tags}\n      auth:\n        kind: oauth2\n        grant: id_jag\n        idp: okta\n        audience: \"{ras}\"\n        resource: \"{url}\"\n        client_id: agentd-at-ras\n        client_auth: {{method: private_key_jwt, key: \"{{{{secret-file:{d}/ras.pem}}}}\"}}\n        scopes: [\"issues:read\"]\n        tool_scopes: {{create_issue: [\"issues:write\"]}}\n",
            ras = x.ras
        )
    };
    format!(
        "agent: {{name: ema, preflight: never}}\nintelligence: {{endpoints: \"mock:final\", model: m}}\nstore: {{kind: file, file: {{path: \"{d}/state\"}}}}\nobservability: {{log_level: info}}\na2a:\n{a2a}security:\n  experimental: {{id_jag: true}}\nidentity:\n  providers:\n    okta:\n      issuer: \"{idp}\"\n      client_id: agentd-idp\n      client_auth: {{method: private_key_jwt, key: \"{{{{secret-file:{d}/idp.pem}}}}\"}}\n      subject: refresh_token\n      sign_in: {{grants: [authorization_code, device]}}\n  delegation:\n{delegation}mcp:\n  servers:\n{read}{gated}{more}workflows:\n{who}{write}{via}{extra}",
        idp = x.idp,
        read = server("linear", &x.mcp, READ_TAGS),
        gated = server("gated", &x.mcp2, o.gated_tags.unwrap_or(GATED_TAGS)),
        more = fill(&o.servers_extra),
        extra = fill(&o.extra),
        who = wf("who", "linear", "whoami", "{}"),
        write = wf("write", "gated", "create_issue", "{title: \"From agentd\"}"),
        // A run that starts `who` as its child: the child acts through the
        // session its parent does.
        via = "  - name: via\n    steps:\n      start: {kind: manual}\n      child: {kind: workflow, depends_on: [start], name: who, mode: sync}\n      done: {kind: finish, depends_on: [child]}\n",
    )
}

/// A daemon started by hand: stopped as the harness stops its own.
pub(super) struct Framed(Child);

impl Drop for Framed {
    fn drop(&mut self) {
        // SAFETY: a signal to the child this guard owns.
        unsafe {
            libc::kill(self.0.id() as i32, libc::SIGTERM);
        }
        std::thread::sleep(Duration::from_millis(300));
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

/// A URL's `host:port` and its path with the query.
fn split(url: &str) -> (String, String) {
    let rest = url.strip_prefix("http://").unwrap_or(url);
    match rest.split_once('/') {
        Some((host, path)) => (host.to_string(), format!("/{path}")),
        None => (rest.to_string(), "/".to_string()),
    }
}

/// A form POST to `path` on `addr`: status and JSON body.
fn form_post(addr: &str, path: &str, form: &str) -> (u16, Value) {
    let (status, _, body) = util::exchange(
        addr,
        "POST",
        path,
        &[("Content-Type", "application/x-www-form-urlencoded")],
        form,
    );
    (
        status,
        serde_json::from_str(&body).unwrap_or(json!({"unparsed": body})),
    )
}

/// A person's browser following start link `uri`: where the start link sent
/// it (the IdP's authorization URL), and the cookie it set.
pub(super) fn open_start(uri: &str) -> Result<(String, String), String> {
    let (addr, path) = split(uri);
    let (status, headers, body) = util::exchange(&addr, "GET", &path, &[], "");
    let location = header(&headers, "location").map(str::to_string);
    let cookie = header(&headers, "set-cookie")
        .and_then(|c| c.split(';').next())
        .map(str::to_string);
    match (status, location, cookie) {
        (302, Some(l), Some(c)) => Ok((l, c)),
        _ => Err(format!(
            "the start link {uri} answered {status}: {headers:?} {body}"
        )),
    }
}

/// The browser at the IdP, which signs the person in at once: where it is
/// sent back to.
pub(super) fn at_idp(url: &str) -> Result<String, String> {
    let (addr, path) = split(url);
    let (status, headers, body) = util::exchange(&addr, "GET", &path, &[], "");
    match (status, header(&headers, "location")) {
        (302, Some(l)) => Ok(l.to_string()),
        _ => Err(format!("the IdP answered {status}: {body}")),
    }
}

/// The browser back at the callback, with `cookie` or without one: the
/// page's status and text.
pub(super) fn back(url: &str, cookie: Option<&str>) -> (u16, String) {
    let (addr, path) = split(url);
    let headers: Vec<(&str, &str)> = cookie.map(|c| ("Cookie", c)).into_iter().collect();
    let (status, _, body) = util::exchange(&addr, "GET", &path, &headers, "");
    (status, body)
}

/// One person's browser through start link `uri`, start to callback: the
/// callback's status and page.
pub(super) fn sign_in_at(uri: &str) -> Result<(u16, String), String> {
    let (idp, cookie) = open_start(uri)?;
    let callback = at_idp(&idp)?;
    Ok(back(&callback, Some(&cookie)))
}

/// The document a command answered with: a read's message data, or the data
/// of the task it completed as.
pub(super) fn doc(v: &Value) -> Value {
    let r = &v["result"];
    if let Some(d) = r["message"]["parts"][0].get("data") {
        return d.clone();
    }
    r["task"]["artifacts"][0]["parts"][0]["data"].clone()
}

impl Fed {
    pub(super) fn new(h: &Harness, o: &FedOpts) -> Fed {
        let tmp = h.tempdir();
        let xaa = Xaa::spawn(h, &tmp, json!({}));
        std::fs::write(tmp.path().join("idp.pem"), key_pem()).expect("write");
        std::fs::write(tmp.path().join("ras.pem"), key_pem()).expect("write");
        std::fs::write(tmp.path().join("seal"), "ab".repeat(32)).expect("write");
        let ops = format!("ops-{}{}", b64url(&rand16()), b64url(&rand16()));
        std::fs::write(tmp.path().join("ops"), &ops).expect("write");
        let port = util::free_port();
        let cfg = tmp.path().join("c.yaml");
        std::fs::write(&cfg, fed_config(&xaa, tmp.path(), port, o)).expect("write");
        let log = tmp.path().join("daemon.log");
        Fed {
            tmp,
            xaa,
            cfg,
            port,
            ops,
            log,
        }
    }

    pub(super) fn addr(&self) -> String {
        format!("127.0.0.1:{}", self.port)
    }

    pub(super) fn cfg(&self) -> &str {
        self.cfg.to_str().expect("utf8")
    }

    /// The daemon, its listener answering; its log appended to.
    pub(super) fn daemon(&self, h: &Harness) -> crate::harness::Daemon {
        let n = std::fs::read_dir(self.tmp.path())
            .map(|d| d.count())
            .unwrap_or(0);
        let log = self.tmp.path().join(format!("daemon-{n}.log"));
        let d = h.spawn_logged(&["--config", self.cfg()], &log);
        self.listening(&log);
        d
    }

    /// The daemon, every frame it sends a child written to `frames`.
    pub(super) fn daemon_framed(&self, h: &Harness, frames: &Path) -> Framed {
        let log = self.tmp.path().join("daemon-framed.log");
        let child = Command::new(h.agentd())
            .args(["--config", self.cfg()])
            .env("AGENTD_TEST_FRAMES_FILE", frames)
            .stdout(Stdio::null())
            .stderr(std::fs::File::create(&log).expect("log"))
            .spawn()
            .expect("spawn");
        let framed = Framed(child);
        self.listening(&log);
        framed
    }

    /// Wait for the listener to answer, then read the daemon's log at `log`.
    pub(super) fn listening(&self, log: &Path) {
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let (status, _, _) = if std::net::TcpStream::connect(self.addr()).is_ok() {
                util::exchange(&self.addr(), "GET", "/.well-known/agent-card.json", &[], "")
            } else {
                (0, Vec::new(), String::new())
            };
            if status == 200 {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the daemon never listened: {}",
                std::fs::read_to_string(log).unwrap_or_default()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
        let _ = std::fs::remove_file(&self.log);
        let _ = std::os::unix::fs::symlink(log, &self.log);
    }

    pub(super) fn log_text(&self) -> String {
        std::fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub(super) fn events(&self, name: &str) -> Vec<Value> {
        self.log_text()
            .lines()
            .filter_map(|l| serde_json::from_str::<Value>(l).ok())
            .filter(|e| e["event"] == name)
            .collect()
    }

    /// `method` as `bearer`: the envelope.
    pub(super) fn rpc_as(&self, bearer: &str, method: &str, params: Value) -> Value {
        let auth = format!("Bearer {bearer}");
        let exts = format!("{}, {}", util::COMMAND, util::ANNOTATIONS);
        let mut headers = vec![("A2A-Extensions", exts.as_str())];
        if !bearer.is_empty() {
            headers.push(("Authorization", &auth));
        }
        let (_, _, body) =
            util::post_full(&self.addr(), &util::rpc_body(1, method, params), &headers);
        serde_json::from_str(&body).unwrap_or(json!({"unparsed": body}))
    }

    /// Command `op` as `bearer`: the envelope.
    pub(super) fn op_as(&self, bearer: &str, op: &str, args: Value) -> Value {
        self.rpc_as(bearer, "SendMessage", util::command_send_params(op, &args))
    }

    /// Command `op` as the operator.
    pub(super) fn operator(&self, op: &str, args: Value) -> Value {
        self.op_as(&self.ops, op, args)
    }

    /// Every live session, as the operator lists them.
    pub(super) fn sessions(&self) -> Vec<Value> {
        doc(&self.operator("auth.sessions", json!({})))["sessions"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    }

    /// Task `id` once it has ended, as `bearer` reads it: its state and its
    /// status text.
    pub(super) fn task_end(&self, bearer: &str, id: &str) -> Result<(String, String), String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let t = self.rpc_as(bearer, "GetTask", json!({"id": id}));
            let state = t["result"]["status"]["state"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let text = t["result"]["status"]["message"]["parts"][0]["text"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            if [
                "TASK_STATE_COMPLETED",
                "TASK_STATE_FAILED",
                "TASK_STATE_CANCELED",
                "TASK_STATE_REJECTED",
            ]
            .contains(&state.as_str())
            {
                return Ok((state, text));
            }
            if Instant::now() > deadline {
                return Err(format!("task {id} did not end: {t}"));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// A display client signing in at `a2a.login` as `sub`: its session
    /// token, once the browser came back to the callback.
    pub(super) fn login(&self, sub: &str) -> Result<String, String> {
        self.xaa.control(json!({"sub": sub}));
        let (status, asked) = form_post(
            &self.addr(),
            "/oauth2/login_authorization",
            "client_id=agentd-web",
        );
        let (Some(start), Some(request)) =
            (asked["start_uri"].as_str(), asked["request_code"].as_str())
        else {
            return Err(format!("login_authorization answered {status}: {asked}"));
        };
        let (page, text) = sign_in_at(start)?;
        if page != 200 {
            return Err(format!("the callback answered {page}: {text}"));
        }
        let (status, token) = form_post(
            &self.addr(),
            "/oauth2/token",
            &format!("grant_type={LOGIN_GRANT}&request_code={request}&client_id=agentd-web"),
        );
        token["access_token"]
            .as_str()
            .map(str::to_string)
            .ok_or_else(|| format!("the login grant answered {status}: {token}"))
    }

    /// A device-grant session the operator approved as `name`: its token and
    /// its sid.
    pub(super) fn device_session(&self, name: &str) -> Result<(String, String), String> {
        let before: Vec<Value> = self.sessions().iter().map(|s| s["sid"].clone()).collect();
        let (_, code) = form_post(
            &self.addr(),
            "/oauth2/device_authorization",
            "client_id=agentd-tui",
        );
        let (Some(dc), Some(uc)) = (code["device_code"].as_str(), code["user_code"].as_str())
        else {
            return Err(format!("device_authorization: {code}"));
        };
        let ok = self.operator("auth.device.approve", json!({"user_code": uc, "as": name}));
        if ok.get("error").is_some() {
            return Err(format!("auth.device.approve: {ok}"));
        }
        let (status, t) = form_post(
            &self.addr(),
            "/oauth2/token",
            &format!(
                "grant_type=urn%3Aietf%3Aparams%3Aoauth%3Agrant-type%3Adevice_code&device_code={dc}&client_id=agentd-tui"
            ),
        );
        let token = t["access_token"]
            .as_str()
            .ok_or_else(|| format!("the device grant answered {status}: {t}"))?
            .to_string();
        let sid = self
            .sessions()
            .iter()
            .map(|s| s["sid"].clone())
            .find(|s| !before.contains(s))
            .and_then(|s| s.as_str().map(str::to_string))
            .ok_or("the new session is not listed")?;
        Ok((token, sid))
    }

    /// `delegation.enrol` from session `bearer` through its start link, the
    /// IdP signing in `sub`: how the enrolment's task ended.
    pub(super) fn enrol_signed_in(
        &self,
        bearer: &str,
        sub: &str,
    ) -> Result<(String, String), String> {
        self.xaa.control(json!({"sub": sub}));
        let r = self.op_as(bearer, "delegation.enrol", json!({"idp": "okta"}));
        let task = &r["result"]["task"];
        let Some(id) = task["id"].as_str() else {
            return Err(format!("delegation.enrol: {r}"));
        };
        let link = task["metadata"][util::ANNOTATIONS]["signIn"]["start_uri"].as_str();
        if let Some(uri) = link {
            sign_in_at(uri)?;
        }
        self.task_end(bearer, id)
    }

    /// `delegation.enrol` from session `bearer` with the IdP's device grant,
    /// the device code approved by `sub`: how the enrolment's task ended.
    pub(super) fn enrol_by_device_code(
        &self,
        bearer: &str,
        sub: &str,
    ) -> Result<(String, String), String> {
        self.xaa.control(json!({"sub": sub}));
        let r = self.op_as(
            bearer,
            "delegation.enrol",
            json!({"idp": "okta", "grant": "device"}),
        );
        let Some(id) = r["result"]["task"]["id"].as_str() else {
            return Err(format!("delegation.enrol: {r}"));
        };
        self.task_end(bearer, id)
    }

    /// What `delegation.list` shows `bearer`.
    pub(super) fn listed(&self, bearer: &str) -> Value {
        doc(&self.op_as(bearer, "delegation.list", json!({})))
    }

    pub(super) fn runs_done(&self, name: &str) -> Vec<Value> {
        self.events("run.done")
            .into_iter()
            .filter(|e| e["workflow"] == name)
            .collect()
    }

    /// Start workflow `name` as `bearer`: how many runs of it had ended.
    pub(super) fn start_as(&self, bearer: &str, name: &str) -> Result<usize, String> {
        let before = self.runs_done(name).len();
        let r = self.op_as(bearer, "workflow.run", json!({"workflow": name}));
        match r.get("error") {
            None => Ok(before),
            Some(_) => Err(format!("workflow.run {name}: {r}")),
        }
    }

    pub(super) fn ended(&self, name: &str, n: usize) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(e) = self.runs_done(name).get(n) {
                return Ok(e.clone());
            }
            if Instant::now() > deadline {
                return Err(format!(
                    "run {n} of {name} did not end:\n{}",
                    tail(&self.log_text())
                ));
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Run `name` as `bearer` to its end.
    pub(super) fn run_as(&self, bearer: &str, name: &str) -> Result<Value, String> {
        let n = self.start_as(bearer, name)?;
        self.ended(name, n)
    }

    /// The gate `bearer`'s own list shows, once one is open.
    pub(super) fn gate_of(&self, bearer: &str) -> Result<Value, String> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let l = self.listed(bearer);
            if let Some(g) = l["gates"].as_array().and_then(|g| g.first()) {
                return Ok(g.clone());
            }
            if Instant::now() > deadline {
                return Err(format!("no gate opened: {l}\n{}", tail(&self.log_text())));
            }
            std::thread::sleep(Duration::from_millis(200));
        }
    }

    /// The subject of each ID-JAG the authorization server was presented, in
    /// order: whose grant each call was made with.
    pub(super) fn minted_for(&self) -> Vec<String> {
        self.xaa
            .grants("ras", JWT_BEARER)
            .iter()
            .filter_map(|r| {
                r["body"]["assertion"]
                    .as_str()
                    .map(|j| claims(j)["sub"].as_str().unwrap_or_default().to_string())
            })
            .collect()
    }
}

fn cfg_refusal(h: &Harness, yaml: &str, dir: &TempDir) -> crate::harness::RunResult {
    let p = dir.path().join("refuse.yaml");
    std::fs::write(&p, yaml).expect("write");
    h.run(&["--config", p.to_str().expect("utf8"), "--validate-config"])
}

// --- the checks ---------------------------------------------------------------

/// `a2a.login`: a display client signs in through a start link; the session
/// it is issued is the person, `user:oidc:<iss>#<sub>`, carrying the IdP
/// proof; and, a template admitting them, the same sign-in enrolled them —
/// their run acts with their own grant.
pub(super) fn a_login_is_the_enrolment(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let token = tryo!(s.login(ALICE));
    let principal = format!("user:oidc:{}#{ALICE}", s.xaa.idp);
    let sessions = s.sessions();
    let theirs = sessions
        .iter()
        .find(|x| x["principal"] == principal.as_str())
        .cloned();
    let listed = s.listed(&token);
    let run = tryo!(s.run_as(&token, "who"));
    let signed_in = s.events("auth.login.signed_in");
    let calls = s.xaa.mcp_calls("tools/call").len();
    let via = tryo!(s.run_as(&token, "via"));
    let child = tryo!(s.ended("who", 1));
    Outcome::require(
        theirs.as_ref().is_some_and(|x| {
            x["source"] == "oidc"
                && x["idp_proof"]["sub"] == ALICE
                && x["idp_proof"]["idp"] == "okta"
        }),
        format!("the session is {principal}, source oidc, with its proof: {sessions:?}"),
    )
    .and(|| {
        Outcome::require(
            listed["grants"].as_array().is_some_and(|g| {
                g.len() == 1
                    && g[0]["principal"] == principal.as_str()
                    && g[0]["template"] == "people"
            }),
            format!("the sign-in kept their grant: {listed}"),
        )
    })
    .and(|| completed(&run))
    .and(|| {
        Outcome::require(
            s.minted_for().last().map(String::as_str) == Some(ALICE),
            "the call was made with their own ID-JAG",
        )
    })
    .and(|| Outcome::require(!signed_in.is_empty(), "the sign-in is logged"))
    .and(|| completed(&via))
    .and(|| completed(&child))
    .and(|| {
        Outcome::require(
            s.xaa.mcp_calls("tools/call").len() > calls,
            "the child run's call is made for the person too",
        )
    })
}

/// Two people sign in: each run acts with its own person's grant — its own
/// exchange, its own subject — and neither lists, nor answers, the other's.
pub(super) fn two_people_two_grants(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let alice = tryo!(s.login(ALICE));
    let bob = tryo!(s.login(BOB));
    let a = tryo!(s.run_as(&alice, "who"));
    let after_alice = s.minted_for();
    let b = tryo!(s.run_as(&bob, "who"));
    let after_bob = s.minted_for();
    let exchanges = s.xaa.grants("idp", EXCHANGE);
    let subjects: std::collections::BTreeSet<&str> = exchanges
        .iter()
        .filter_map(|e| e["body"]["subject_token"].as_str())
        .collect();
    let bobs = s.listed(&bob);
    // Alice's write asks her; Bob, holding her gate's id, is refused it.
    let n = tryo!(s.start_as(&alice, "write"));
    let gate = tryo!(s.gate_of(&alice));
    let id = gate["gate"].as_str().unwrap_or_default();
    let refused = s.op_as(
        &bob,
        "delegation.approve",
        json!({"gate": id, "nonce": gate["nonce"], "args_hash": gate["args_hash"]}),
    );
    let bob_sees = s.listed(&bob);
    let ok = s.op_as(
        &alice,
        "delegation.approve",
        json!({"gate": id, "nonce": gate["nonce"], "args_hash": gate["args_hash"]}),
    );
    let w = tryo!(s.ended("write", n));
    completed(&a)
        .and(|| completed(&b))
        .and(|| {
            Outcome::require(
                after_alice.last().map(String::as_str) == Some(ALICE)
                    && after_bob.last().map(String::as_str) == Some(BOB),
                format!("each call is made for its own person: {after_alice:?} {after_bob:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                subjects.len() >= 2,
                format!("each person's exchange presents their own refresh token: {exchanges:?}"),
            )
        })
        .and(|| {
            Outcome::require(
                bobs["grants"].as_array().is_some_and(|g| {
                    g.len() == 1 && g[0]["principal"].as_str().is_some_and(|p| p.ends_with(BOB))
                }),
                format!("Bob lists his own grant alone: {bobs}"),
            )
        })
        .and(|| {
            Outcome::require(
                refused.get("error").is_some()
                    && bob_sees["gates"].as_array().is_none_or(|g| g.is_empty()),
                format!("Bob may neither answer nor see Alice's gate: {refused} {bob_sees}"),
            )
        })
        .and(|| {
            Outcome::require(
                ok.get("error").is_none(),
                format!("Alice answers her own: {ok}"),
            )
        })
        .and(|| completed(&w))
}

/// A delegated server's tools are offered only in work for a person who
/// holds a usable grant there: after Bob's grant is revoked his turn is
/// handed none of them, while Alice's turn, through her own session, is.
pub(super) fn tools_are_offered_only_with_a_grant(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            ..FedOpts::default()
        },
    );
    let frames = s.tmp.path().join("frames.jsonl");
    let _d = s.daemon_framed(h, &frames);
    let alice = tryo!(s.login(ALICE));
    let bob = tryo!(s.login(BOB));
    let r = s.operator(
        "delegation.revoke",
        json!({"target": format!("idp:okta#{BOB}")}),
    );
    if let Some(id) = r["result"]["task"]["id"].as_str() {
        tryo!(s.task_end(&s.ops, id));
    }
    // What each person's turn is handed, from the frames sent to its worker.
    let turn = |bearer: &str| -> Result<String, String> {
        let before = std::fs::read_to_string(&frames).unwrap_or_default().len();
        let params = json!({"message": {"role": "ROLE_USER", "messageId": b64url(&rand16()),
            "parts": [{"text": "what is open on my board?"}]},
            "configuration": {"returnImmediately": false}});
        let r = s.rpc_as(bearer, "SendMessage", params);
        if r.get("error").is_some() {
            return Err(format!("SendMessage: {r}"));
        }
        std::thread::sleep(Duration::from_millis(500));
        let all = std::fs::read_to_string(&frames).unwrap_or_default();
        Ok(all.get(before..).unwrap_or_default().to_string())
    };
    let bobs = tryo!(turn(&bob));
    let alices = tryo!(turn(&alice));
    Outcome::require(
        !bobs.is_empty() && !alices.is_empty(),
        "each turn's frames were captured",
    )
    .and(|| {
        Outcome::require(
            !bobs.contains("whoami"),
            "a person with no usable grant is offered no tool of a delegated server",
        )
    })
    .and(|| {
        Outcome::require(
            alices.contains("whoami"),
            "the person who holds a grant is offered its tools",
        )
    })
}

/// The IdP's URL a start link sends a browser to, forwarded to someone else,
/// brings their browser back without the start link's cookie: the callback
/// refuses it, the sign-in fails, and nobody is issued a session. A start
/// link opens once.
pub(super) fn a_forwarded_sign_in_fails_at_the_callback(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let (_, asked) = form_post(
        &s.addr(),
        "/oauth2/login_authorization",
        "client_id=agentd-web",
    );
    let start = asked["start_uri"].as_str().unwrap_or_default().to_string();
    let request = asked["request_code"]
        .as_str()
        .unwrap_or_default()
        .to_string();
    // The one who asked opens the link; the IdP URL goes to the victim.
    let (idp_url, _cookie) = tryo!(open_start(&start));
    let again = util::exchange(&s.addr(), "GET", &split(&start).1, &[], "").0;
    s.xaa.control(json!({"sub": BOB}));
    let callback = tryo!(at_idp(&idp_url));
    let (refused, page) = back(&callback, None);
    let (_, polled) = form_post(
        &s.addr(),
        "/oauth2/token",
        &format!("grant_type={LOGIN_GRANT}&request_code={request}&client_id=agentd-web"),
    );
    let sessions = s.sessions();
    Outcome::require(
        again == 410 || again == 404,
        format!("a start link opens once: {again}"),
    )
    .and(|| {
        Outcome::require(
            refused == 403 && page.contains("did not come back to the browser that started it"),
            format!("the callback without the cookie: {refused} {page}"),
        )
    })
    .and(|| {
        Outcome::require(
            polled["error"] == "access_denied",
            format!("the sign-in failed: {polled}"),
        )
    })
    .and(|| {
        Outcome::require(
            sessions.iter().all(|x| x["source"] != "oidc"),
            format!("nobody was signed in: {sessions:?}"),
        )
    })
}

/// With no template for them, a person signs in all the same and is kept no
/// grant — the refresh token is revoked at the IdP — and their run is
/// not_enrolled; a device session no template admits is refused its
/// enrolment before anyone is sent to the IdP.
pub(super) fn no_template_no_grant(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            device: true,
            // A template, but for the operator's people alone.
            templates: vec![
                "{name: eng, principals: {source: operator}, idp: okta, servers: {linear: {scopes: [\"issues:read\"]}}}",
            ],
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let revokes = |s: &Fed| {
        s.xaa
            .record()
            .iter()
            .filter(|r| r["at"] == "idp" && r["path"] == "/revoke")
            .count()
    };
    let token = tryo!(s.login(ALICE));
    let listed = s.listed(&token);
    let revoked = revokes(&s);
    let run = tryo!(s.run_as(&token, "who"));
    let (device, _) = tryo!(s.device_session("laptop"));
    let asked = s.xaa.record().len();
    let (state, text) = tryo!(s.enrol_by_device_code(&device, ALICE));
    let asked_after = s
        .xaa
        .record()
        .iter()
        .filter(|r| r["path"] == "/device")
        .count();
    Outcome::require(
        listed["grants"].as_array().is_none_or(|g| g.is_empty()),
        format!("no grant is kept: {listed}"),
    )
    .and(|| Outcome::require(revoked >= 1, "the refresh token is revoked at the IdP"))
    .and(|| failed_with(&run, "not_enrolled"))
    .and(|| {
        Outcome::require(
            state == "TASK_STATE_FAILED"
                && text.contains("no identity.delegation.templates entry admits"),
            format!("the device's enrolment: {state} {text}"),
        )
    })
    .and(|| {
        Outcome::require(
            asked_after == 0 && s.xaa.record().len() >= asked,
            "no device code was asked of the IdP",
        )
    })
}

/// An unpinned device name delegates only with device_first_binding: off,
/// its enrolment is refused before the IdP is asked anything.
pub(super) fn an_unpinned_device_needs_first_binding(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            device: true,
            templates: vec![DEVICES],
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let (device, _) = tryo!(s.device_session("laptop"));
    let (state, text) = tryo!(s.enrol_by_device_code(&device, ALICE));
    let (state2, text2) = tryo!(s.enrol_signed_in(&device, ALICE));
    let asked = s
        .xaa
        .record()
        .iter()
        .filter(|r| {
            r["path"] == "/device"
                || r["path"]
                    .as_str()
                    .is_some_and(|p| p.starts_with("/authorize"))
        })
        .count();
    Outcome::require(
        state == "TASK_STATE_FAILED"
            && text.contains("principal_not_delegable")
            && text.contains("device_first_binding"),
        format!("by device code: {state} {text}"),
    )
    .and(|| {
        Outcome::require(
            state2 == "TASK_STATE_FAILED" && text2.contains("principal_not_delegable"),
            format!("through a start link: {state2} {text2}"),
        )
    })
    .and(|| Outcome::require(asked == 0, "the IdP was asked nothing"))
}

/// A pinned device name binds only the identity its pin names: a device
/// code the IdP approved for another subject is binding_mismatch and keeps
/// nothing; signed in as the pin, it is bound, and its session's run acts
/// with the grant.
pub(super) fn a_pinned_device_binds_its_pin_alone(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            device: true,
            templates: vec![DEVICES],
            pins: Some("{laptop: {iss: \"__ISS__\", sub: 00u1alice}}"),
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let (device, _) = tryo!(s.device_session("laptop"));
    let (state, text) = tryo!(s.enrol_by_device_code(&device, BOB));
    let nothing = s.listed(&s.ops);
    let before = tryo!(s.run_as(&device, "who"));
    let (state2, text2) = tryo!(s.enrol_signed_in(&device, ALICE));
    let listed = s.listed(&s.ops);
    let run = tryo!(s.run_as(&device, "who"));
    Outcome::require(
        state == "TASK_STATE_FAILED" && text.contains("binding_mismatch"),
        format!("a device code another subject approved: {state} {text}"),
    )
    .and(|| {
        Outcome::require(
            nothing["grants"].as_array().is_none_or(|g| g.is_empty()),
            format!("nothing was kept: {nothing}"),
        )
    })
    .and(|| failed_with(&before, "not_enrolled"))
    .and(|| {
        Outcome::require(
            state2 == "TASK_STATE_COMPLETED",
            format!("signed in as the pin: {state2} {text2}"),
        )
    })
    .and(|| {
        Outcome::require(
            listed["grants"]
                .as_array()
                .is_some_and(|g| g.len() == 1 && g[0]["sub"] == ALICE && g[0]["bound_by"] == "pin"),
            format!("bound by the pin: {listed}"),
        )
    })
    .and(|| completed(&run))
}

/// Two devices approved under one name, first binding on: only the one that
/// signed in uses the grant — the other is not_enrolled — and its sign-in as
/// someone else is binding_mismatch, the grant untouched.
pub(super) fn one_device_name_one_binding(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            device: true,
            templates: vec![DEVICES],
            first_binding: true,
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let (first, _) = tryo!(s.device_session("laptop"));
    let (second, _) = tryo!(s.device_session("laptop"));
    let (state, text) = tryo!(s.enrol_signed_in(&first, ALICE));
    let bound = s.listed(&s.ops);
    let other = tryo!(s.run_as(&second, "who"));
    let (state2, text2) = tryo!(s.enrol_signed_in(&second, BOB));
    let after = s.listed(&s.ops);
    let still = tryo!(s.run_as(&first, "who"));
    let grant = |l: &Value| l["grants"][0].clone();
    Outcome::require(
        state == "TASK_STATE_COMPLETED",
        format!("the first binds: {state} {text}"),
    )
    .and(|| {
        Outcome::require(
            grant(&bound)["sub"] == ALICE && grant(&bound)["bound_by"] == "first_binding",
            format!("bound by the first enrolment: {bound}"),
        )
    })
    .and(|| failed_with(&other, "not_enrolled"))
    .and(|| {
        Outcome::require(
            state2 == "TASK_STATE_FAILED" && text2.contains("binding_mismatch"),
            format!("another subject at the same name: {state2} {text2}"),
        )
    })
    .and(|| {
        Outcome::require(
            after["grants"].as_array().is_some_and(|g| g.len() == 1)
                && grant(&after)["sub"] == ALICE
                && grant(&after)["epoch"] == grant(&bound)["epoch"],
            format!("the grant is untouched: {bound} then {after}"),
        )
    })
    .and(|| completed(&still))
}

/// A person's gate is answered only through a session that signed in at the
/// IdP as them, recently: a device session under the same name that never
/// signed in is refused (`no_idp_sign_in`), the person's own once its sign-in
/// is older than `consent_session_max_age` (`stale_session`), and the answer
/// is taken once they sign in again (`via: gate`); an operator the template
/// lists among its approvers answers too (`via: operator`).
pub(super) fn a_gate_is_answered_through_a_fresh_sign_in(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            device: true,
            templates: vec![
                "{name: devices, principals: {source: device}, idp: okta, approvers: [principal, operator], servers: {linear: {scopes: [\"issues:read\"]}, gated: {scopes: [\"issues:read\", \"issues:write\"]}}}",
            ],
            first_binding: true,
            consent_max_age: Some("3s"),
            ..FedOpts::default()
        },
    );
    let _d = s.daemon(h);
    let (first, _) = tryo!(s.device_session("laptop"));
    let (second, _) = tryo!(s.device_session("laptop"));
    let (state, text) = tryo!(s.enrol_signed_in(&first, ALICE));
    if state != "TASK_STATE_COMPLETED" {
        return Outcome::fail(format!("the enrolment: {state} {text}"));
    }
    let answer = |bearer: &str, g: &Value| {
        s.op_as(
            bearer,
            "delegation.approve",
            json!({"gate": g["gate"], "nonce": g["nonce"], "args_hash": g["args_hash"]}),
        )
    };
    let n = tryo!(s.start_as(&first, "write"));
    let g = tryo!(s.gate_of(&first));
    let unproved = answer(&second, &g);
    std::thread::sleep(Duration::from_secs(4));
    let stale = answer(&first, &g);
    let (again, _) = tryo!(s.enrol_signed_in(&first, ALICE));
    let fresh = answer(&first, &g);
    let w = tryo!(s.ended("write", n));
    let m = tryo!(s.start_as(&first, "write"));
    let og = tryo!(s.gate_of(&s.ops));
    let by_operator = answer(&s.ops, &og);
    let w2 = tryo!(s.ended("write", m));
    let approved = |via: &str| {
        s.events("delegation.consent")
            .iter()
            .any(|e| e["decision"] == "approved" && e["via"] == via)
    };
    let refused = |r: &Value, says: &str| {
        r["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains(says))
    };
    Outcome::require(
        refused(&unproved, "has not signed in at the IdP"),
        format!("a session that never signed in: {unproved}"),
    )
    .and(|| {
        Outcome::require(
            refused(&stale, "consent_session_max_age"),
            format!("a sign-in older than the limit: {stale}"),
        )
    })
    .and(|| Outcome::require(again == "TASK_STATE_COMPLETED", "the person signs in again"))
    .and(|| {
        Outcome::require(
            fresh.get("error").is_none(),
            format!("the fresh answer: {fresh}"),
        )
    })
    .and(|| completed(&w))
    .and(|| Outcome::require(approved("gate"), "the person's answer is via: gate"))
    .and(|| {
        Outcome::require(
            og["nonce"].as_str().is_some_and(|n| !n.is_empty())
                && by_operator.get("error").is_none(),
            format!("the operator the template lists: {og} {by_operator}"),
        )
    })
    .and(|| completed(&w2))
    .and(|| {
        Outcome::require(
            approved("operator"),
            "the operator's answer is via: operator",
        )
    })
}

/// Ending a session ends what it delegated at once: the gate its run waits
/// on closes `revoked`, the call never made; and a session-bound run's gate
/// that a restart finds closes `revoked` too, since the restart ended every
/// session.
pub(super) fn an_ended_session_ends_its_delegation(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            ..FedOpts::default()
        },
    );
    let creates = |s: &Fed| {
        s.xaa
            .mcp_calls("tools/call")
            .iter()
            .filter(|c| c["body"]["params"]["name"] == "create_issue")
            .count()
    };
    let first = {
        let _d = s.daemon(h);
        let token = tryo!(s.login(ALICE));
        let n = tryo!(s.start_as(&token, "write"));
        tryo!(s.gate_of(&token));
        let sid = s
            .sessions()
            .iter()
            .find(|x| x["source"] == "oidc")
            .and_then(|x| x["sid"].as_str().map(str::to_string))
            .unwrap_or_default();
        let revoked = s.operator("auth.sessions.revoke", json!({"sid": sid}));
        if revoked.get("error").is_some() {
            return Outcome::fail(format!("auth.sessions.revoke: {revoked}"));
        }
        let run = tryo!(s.ended("write", n));
        // Another gate, left open across a restart.
        let again = tryo!(s.login(ALICE));
        tryo!(s.start_as(&again, "write"));
        tryo!(s.gate_of(&again));
        run
    };
    // The restarted daemon's log is a new one: the run's end is its first.
    let _d = s.daemon(h);
    let second = tryo!(s.ended("write", 0));
    failed_with(&first, "revoked")
        .and(|| failed_with(&second, "revoked"))
        .and(|| Outcome::require(creates(&s) == 0, "no call was made for an ended session"))
}

/// A gate the per-person operator's run waits on survives a restart: it is
/// listed again, and their approval makes the call.
pub(super) fn a_gate_survives_a_restart(h: &Harness) -> Outcome {
    let o = Opts {
        tags: "[sensitive, egress]",
        ..Opts::default()
    };
    let s = Setup::new(h, json!({}), &o);
    let (gate, n) = {
        let _d = s.daemon(h);
        tryo!(s.login(h));
        let n = s.start("write");
        (tryo!(wait_gate(h, &s)), n)
    };
    let _d = s.daemon(h);
    let again = tryo!(wait_gate(h, &s));
    let ok = s.cli(h, &["--delegation-approve", &again]);
    let run = tryo!(s.ended("write", n, Duration::from_secs(30)));
    let created = s
        .xaa
        .mcp_calls("tools/call")
        .iter()
        .any(|c| c["body"]["params"]["name"] == "create_issue");
    Outcome::require(
        gate == again,
        format!("the same gate is asked again: {gate} then {again}"),
    )
    .and(|| {
        Outcome::require(
            ok.code == Some(0),
            format!("--delegation-approve: {} {}", ok.stdout, ok.stderr),
        )
    })
    .and(|| completed(&run))
    .and(|| Outcome::require(created, "the approved call is made"))
}

/// The listener rules of `a2a.login`: refused beside mTLS, and a workflow
/// acting for a device principal is refused; a login-only listener declares
/// bearer, grants a loopback caller nothing, and serves `/oauth2/revoke`
/// and its login endpoint, with no device grant.
pub(super) fn the_login_listener_rules(h: &Harness) -> Outcome {
    let s = Fed::new(
        h,
        &FedOpts {
            login: true,
            templates: vec![PEOPLE],
            ..FedOpts::default()
        },
    );
    let base = std::fs::read_to_string(&s.cfg).expect("read");
    // Login-only: no operator bearer either.
    let only = base.replace(
        &format!(
            "  bearer: \"{{{{secret-file:{}/ops}}}}\"\n",
            s.tmp.path().display()
        ),
        "",
    );
    let mtls = base.replace(
        "  login: {idp: okta}\n",
        &format!(
            "  login: {{idp: okta}}\n  tls: {{cert: \"{}\", key: \"{}\", client_ca: \"{}\"}}\n",
            crate::harness::tls_fixture("server.pem").display(),
            crate::harness::tls_fixture("server.key").display(),
            crate::harness::tls_fixture("ca.pem").display()
        ),
    );
    let act_for = format!(
        "{base}  - name: theirs\n    act_for: {{principal: {{source: device, name: laptop}}}}\n    steps:\n      start: {{kind: manual}}\n      done: {{kind: finish, depends_on: [start]}}\n"
    );
    let r_mtls = cfg_refusal(h, &mtls, &s.tmp);
    let r_act = cfg_refusal(h, &act_for, &s.tmp);
    std::fs::write(&s.cfg, &only).expect("write");
    let r_only = s_validate(h, &s);
    let _d = s.daemon(h);
    let card = util::get_card(&s.addr());
    let schemes = card["securitySchemes"].clone();
    let (anon, _, _) = util::post_full(
        &s.addr(),
        &util::rpc_body(
            1,
            "SendMessage",
            util::command_send_params("status", &json!({})),
        ),
        &[("A2A-Extensions", util::COMMAND)],
    );
    let (_, _, meta) = util::exchange(
        &s.addr(),
        "GET",
        "/.well-known/oauth-authorization-server",
        &[],
        "",
    );
    let meta: Value = serde_json::from_str(&meta).unwrap_or(Value::Null);
    let (revoke, _) = form_post(
        &s.addr(),
        "/oauth2/revoke",
        "token=agentd_at_nothing&client_id=agentd-web",
    );
    Outcome::require(
        r_mtls.code == Some(2)
            && r_mtls.stderr.contains("a2a.login")
            && r_mtls.stderr.contains("client_ca"),
        format!(
            "a2a.login beside a2a.tls.client_ca: {:?} {}",
            r_mtls.code, r_mtls.stderr
        ),
    )
    .and(|| {
        Outcome::require(
            r_act.code == Some(2) && r_act.stderr.contains("act_for_device_principal"),
            format!(
                "act_for a device principal: {:?} {}",
                r_act.code, r_act.stderr
            ),
        )
    })
    .and(|| {
        Outcome::require(
            r_only.code == Some(0),
            format!("login alone loads: {}", r_only.stderr),
        )
    })
    .and(|| {
        Outcome::require(
            schemes.as_object().is_some_and(|m| {
                m.values().any(|v| {
                    v["httpAuthSecurityScheme"]["scheme"]
                        .as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case("bearer"))
                })
            }),
            format!("the card declares bearer: {schemes}"),
        )
    })
    .and(|| {
        Outcome::require(
            anon == 401,
            format!("a loopback caller with nothing is refused: {anon}"),
        )
    })
    .and(|| {
        Outcome::require(
            meta["login_authorization_endpoint"]
                .as_str()
                .is_some_and(|u| u.ends_with("/oauth2/login_authorization"))
                && meta.get("device_authorization_endpoint").is_none()
                && meta["revocation_endpoint"].as_str().is_some(),
            format!("the issuer metadata: {meta}"),
        )
    })
    .and(|| Outcome::require(revoke == 200, format!("/oauth2/revoke is served: {revoke}")))
}

fn s_validate(h: &Harness, s: &Fed) -> crate::harness::RunResult {
    h.run(&["--config", s.cfg(), "--validate-config"])
}

/// A reload that narrows the person's template holds their grant to it at
/// the next call; one that removes it revokes the grant at once — at the
/// IdP, tombstoned — and their work is `revoked`.
pub(super) fn a_reload_narrows_then_revokes(h: &Harness) -> Outcome {
    let s = Setup::new(h, json!({}), &Opts::default());
    let _d = s.daemon(h);
    tryo!(s.login(h));
    let wide = tryo!(s.run("write"));
    // A reload is SIGHUP to the daemon, whose every log line names its pid.
    let pid = s
        .log_events()
        .iter()
        .find_map(|e| e["pid"].as_i64())
        .unwrap_or_default() as i32;
    let reloaded = |s: &Setup, n: usize| -> Result<(), String> {
        // SAFETY: a signal to the daemon this check started.
        unsafe {
            libc::kill(pid, libc::SIGHUP);
        }
        let deadline = Instant::now() + Duration::from_secs(20);
        while s.events("config.reloaded").len() <= n {
            if Instant::now() > deadline {
                return Err(format!("no reload:\n{}", tail(&s.log_text())));
            }
            std::thread::sleep(Duration::from_millis(100));
        }
        Ok(())
    };
    let n = s.events("config.reloaded").len();
    s.reconfigure(&Opts {
        template: Some("linear: {scopes: [\"issues:read\"]}"),
        ..Opts::default()
    });
    tryo!(reloaded(&s, n));
    let narrowed = tryo!(s.run("write"));
    let read = tryo!(s.run("who"));
    let n = s.events("config.reloaded").len();
    let cfg = std::fs::read_to_string(&s.cfg)
        .expect("read")
        .replace("{name: eng,", "{name: eng2,");
    std::fs::write(&s.cfg, cfg).expect("write");
    tryo!(reloaded(&s, n));
    let deadline = Instant::now() + Duration::from_secs(20);
    while s.events("delegation.revoked").is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(100));
    }
    let revoked = s.events("delegation.revoked");
    let gone = tryo!(s.run("who"));
    completed(&wide)
        .and(|| failed_with(&narrowed, "scope_beyond_ceiling"))
        .and(|| completed(&read))
        .and(|| {
            Outcome::require(
                revoked.iter().any(|r| {
                    r["why"]
                        .as_str()
                        .is_some_and(|w| w.contains("template eng was removed"))
                        && r["tombstoned"] == true
                        && r["at_idp"] == true
                }),
                format!("the removal revokes, at the IdP, tombstoned: {revoked:?}"),
            )
        })
        .and(|| failed_with(&gone, "revoked"))
}

/// With `subagents: inherit` in the person's template, a run a subagent
/// starts acts with the person's grant at the servers the subagent declared
/// — never another — and the subagent holds no token: what it is answered
/// carries none.
pub(super) fn a_subagent_inherits_what_it_declares(h: &Harness) -> Outcome {
    let tmp = h.tempdir();
    let playbook = json!({"turns": [
        {"tool_calls": [{"name": "file_issue", "arguments": {}}, {"name": "file_other", "arguments": {}}]},
        {"content": "done"}]});
    let pb = crate::checks::util::write_file(&tmp, "playbook.json", &playbook.to_string());
    let llm = h.mock_llm(&format!("file:{pb}"));
    let spawner = "  - name: file\n    tool: {name: file_issue, mode: sync, grant: {subagents: true}}\n    steps:\n      start: {kind: manual}\n      call: {kind: mcp.tool, depends_on: [start], server: linear, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [call], output: \"filed\"}\n  - name: other\n    tool: {name: file_other, mode: sync, grant: {subagents: true}}\n    steps:\n      start: {kind: manual}\n      call: {kind: mcp.tool, depends_on: [start], server: linear2, tool: whoami, args: {}}\n      done: {kind: finish, depends_on: [call]}\n  - name: spawner\n    steps:\n      start: {kind: manual}\n      c: {kind: subagent, depends_on: [start], instruction: \"File it.\", tools: [file_issue, file_other], servers: [linear]}\n      done: {kind: finish, depends_on: [c]}\n";
    let o = Opts {
        two_servers: true,
        extra: spawner.to_string(),
        ..Opts::default()
    };
    let s = Setup::new(h, json!({}), &o);
    let cfg = std::fs::read_to_string(&s.cfg)
        .expect("read")
        .replace(
            "intelligence: {endpoints: \"mock:final\", model: m}",
            &format!("intelligence: {{endpoints: \"{}\", model: m}}", llm.uri),
        )
        .replace(
            "security:\n  experimental: {id_jag: true}\n",
            "security:\n  experimental: {id_jag: true}\n  policies:\n    - {match: {tool: \"file_*\", caller: [subagent]}, action: allow}\n",
        )
        .replace("principals: {source: operator}, idp: okta,", "principals: {source: operator}, idp: okta, subagents: inherit,");
    std::fs::write(&s.cfg, cfg).expect("write");
    let frames = s.tmp.path().join("frames.jsonl");
    let log = s.tmp.path().join("daemon-frames.log");
    let _ = std::fs::remove_file(&s.sock);
    let child = Command::new(h.agentd())
        .args(["--config", s.cfg()])
        .env("AGENTD_TEST_FRAMES_FILE", &frames)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&log).expect("log"))
        .spawn()
        .expect("spawn");
    struct Kill(Child);
    impl Drop for Kill {
        fn drop(&mut self) {
            // SAFETY: a signal to the child this guard owns.
            unsafe {
                libc::kill(self.0.id() as i32, libc::SIGTERM);
            }
            std::thread::sleep(Duration::from_millis(300));
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
    let _k = Kill(child);
    let deadline = Instant::now() + Duration::from_secs(15);
    while !s.sock.exists() {
        if Instant::now() > deadline {
            return Outcome::fail("the daemon never listened");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&s.log);
    let _ = std::os::unix::fs::symlink(&log, &s.log);
    tryo!(s.login(h));
    s.start("spawner");
    let file = tryo!(s.ended("file", 0, Duration::from_secs(60)));
    let other = tryo!(s.ended("other", 0, Duration::from_secs(60)));
    std::thread::sleep(Duration::from_secs(1));
    let mut secrets = Vec::new();
    for r in s.xaa.record() {
        for k in ["subject_token", "assertion", "refresh_token"] {
            if let Some(v) = r["body"][k].as_str() {
                secrets.push(v.to_string());
            }
        }
        if let Some(a) = r["authorization"]
            .as_str()
            .and_then(|a| a.strip_prefix("Bearer "))
        {
            secrets.push(a.to_string());
        }
    }
    let frames_text = std::fs::read_to_string(&frames).unwrap_or_default();
    let leaked = secrets
        .iter()
        .any(|t| t.len() > 8 && frames_text.contains(t.as_str()));
    let calls = s.xaa.mcp_calls("tools/call");
    completed(&file)
        .and(|| failed_with(&other, "delegation_not_permitted"))
        .and(|| {
            Outcome::require(
                calls.iter().all(|c| c["path"] != "/mcp2"),
                "nothing reaches the server the subagent did not declare",
            )
        })
        .and(|| {
            Outcome::require(
                !frames_text.is_empty(),
                "the frames to the children were captured",
            )
        })
        .and(|| Outcome::require(!leaked, "no token reaches a child"))
}
