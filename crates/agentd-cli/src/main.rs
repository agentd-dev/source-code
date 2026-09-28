// SPDX-License-Identifier: AGPL-3.0-only
//! agentd entry point.
//!
//! Dispatches between three roles of the one binary: the **supervisor** (parse
//! and validate the configuration, then run the durable event loop), the
//! **subagent** re-exec, and the early-exit asks (`--help` / `--version` /
//! `--config-schema` / `--validate-config` / `--capabilities`). Which role runs
//! is decided entirely from argv and the environment, before any config is read.

use agentd::config::ConfigError;
use agentd::exit;
use serde_json::json;

#[cfg(all(unix, feature = "a2a"))]
mod launcher;

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    let argv: Vec<String> = std::env::args().collect();

    // `agentd tui …` / `agentd ui …`: the thin launcher — the daemon exactly as
    // `agentd …` would run it, plus a display client beside it that is handed
    // nothing but its endpoint, lifetimes tied so neither survives the other.
    if let Some(sub @ ("tui" | "ui")) = argv.get(1).map(String::as_str) {
        #[cfg(all(unix, feature = "a2a"))]
        {
            let env: Vec<(String, String)> = std::env::vars().collect();
            return launcher::run(sub, &argv[2..], &env);
        }
        // The launcher hands its fds to the client (unix) and dials the A2A
        // listener (the `a2a` feature); without either there is nothing it
        // could launch against.
        #[cfg(not(all(unix, feature = "a2a")))]
        {
            eprintln!(
                "agentd {sub}: this build has no launcher (it needs unix and the a2a feature); start the daemon with `agentd -c …`, then run `agentd-{sub} --endpoint <url>` against it"
            );
            return exit::USAGE;
        }
    }

    // Hidden built-in Streamable HTTP mock MCP server (tests/dev):
    // `--internal-mock-mcp-http <addr-file> <uri> [--no-emit]`. Binds loopback
    // TCP (127.0.0.1:0) and announces the bound address through <addr-file>.
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if argv.get(1).map(String::as_str) == Some("--internal-mock-mcp-http") {
        let addr_file = argv
            .get(2)
            .map(String::as_str)
            .unwrap_or("/tmp/agentd-mock-mcp.addr");
        let uri = argv.get(3).map(String::as_str).unwrap_or("mock://resource");
        let emit = !argv.iter().any(|a| a == "--no-emit");
        return agentd::mcp::mock_http::run(addr_file, uri, emit);
    }

    // Hidden built-in mock LLM (tests):
    // `--internal-mock-llm <addr-file> [final|read|schedule|file:<playbook>]`.
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if argv.get(1).map(String::as_str) == Some("--internal-mock-llm") {
        let addr_file = argv
            .get(2)
            .map(String::as_str)
            .unwrap_or("/tmp/agentd-mock-llm.addr");
        let script = argv.get(3).map(String::as_str).unwrap_or("final");
        return agentd::intel::mock::run(addr_file, script);
    }

    // Subagent re-exec dispatch. The supervisor sets this in the child's
    // environment; the child reads its spawn payload over the control channel
    // (stdin) rather than from CLI/env config.
    if std::env::var_os(agentd::subagent::protocol::SUBAGENT_ENV).is_some() {
        return agentd::subagent::control::run();
    }

    // An instance-tier child is a NORMAL daemon (`--config` composed by its
    // parent) — the only difference is that a parent death should retire it
    // gracefully rather than leave it orphaned and running.
    if std::env::var_os(agentd::supervisor::reap::INSTANCE_CHILD_ENV).is_some() {
        agentd::supervisor::reap::install_instance_pdeathsig();
    }

    let env: Vec<(String, String)> = std::env::vars().collect();
    run_v2(&argv[1..], &env)
}

/// A loaded daemon invocation: the configuration, what was asked of it, and
/// the arguments and environment it was loaded from — the per-process intents
/// (`--fresh`, `--prompt-missing`, `--env`) already consumed.
pub(crate) struct Invocation {
    pub loaded: agentd::config::v2::Loaded,
    pub ask: agentd::config::v2::Ask,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The agentd supervisor: load + validate the configuration and run it (or
/// answer an early-exit ask). A flat-schema (or `--mode`) configuration is
/// rejected with a migration hint; `v2::load` emits the precise per-key
/// diagnostics that say which entries are unrecognized.
fn run_v2(args: &[String], env: &[(String, String)]) -> i32 {
    use agentd::config::v2::{self, Ask};
    let Invocation {
        loaded,
        ask,
        args,
        env,
    } = match load_invocation(args, env) {
        Ok(inv) => inv,
        Err(code) => return code,
    };
    let (args, env) = (args.as_slice(), env.as_slice());
    match ask {
        Ask::Help => {
            // `--fresh` never reaches the settings model, so it is not in the
            // generated flag tables either — splice it into the CONTROL block, or
            // it would be a flag that works and is undocumented.
            print!(
                "{}",
                v2::help_text().replace(
                    "  -h, --help",
                    "  --fresh                    start a NEW generation: do not resume prior durable state\n                             (the previous generation is kept on the store, not deleted)\n  -h, --help",
                )
            );
            exit::SUCCESS
        }
        Ask::Version => {
            println!("agentd {}", agentd::VERSION);
            exit::SUCCESS
        }
        Ask::Schema => {
            println!(
                "{}",
                serde_json::to_string_pretty(&v2::schema::schema())
                    .unwrap_or_else(|_| "{}".to_string())
            );
            exit::SUCCESS
        }
        Ask::ContextTemplate => {
            // The built-in system-prompt template. Printing it is how an
            // operator writing an override starts from the real thing rather
            // than an approximation of it.
            println!("{}", agentd::runtime::env::DEFAULT_TEMPLATE);
            exit::SUCCESS
        }
        Ask::WorkflowSchema => {
            println!(
                "{}",
                serde_json::to_string_pretty(&agentd::engine::workflow_schema())
                    .unwrap_or_else(|_| "{}".to_string())
            );
            exit::SUCCESS
        }
        Ask::Validate => {
            for w in &loaded.warnings {
                eprintln!("{}", json!({"event": "config.warning", "msg": w}));
            }
            // Review reads the OUTCOME, not the input: report the effective
            // tool surface and tag set per consumer AFTER catalog resolution,
            // since that is what actually governs at run time.
            for s in &loaded.settings.mcp.servers {
                if s.service.is_some() || !loaded.settings.services.is_empty() {
                    eprintln!(
                        "{}",
                        json!({"event": "config.effective_server", "server": s.name,
                               "service": s.service, "endpoint": s.endpoint,
                               "allow": s.allow, "exclude": s.exclude, "tags": s.tags})
                    );
                }
            }
            eprintln!(
                "{}",
                // The document version this config was validated against, from
                // the one constant that defines it — it was hardcoded "2" and
                // kept saying so after the reset to `config_version: "1"`,
                // which is exactly the kind of drift a literal invites.
                json!({"event": "config.valid",
                       "schema": agentd::config::v2::schema::CONFIG_VERSION,
                       "files": loaded.files.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>()})
            );
            exit::SUCCESS
        }
        // `--effective-config`: the assembled document, where each setting came
        // from, and the fragment an instruction declared. On stdout, so it
        // pipes into `jq`; the warnings stay on stderr with every other event.
        Ask::EffectiveConfig => {
            for w in &loaded.warnings {
                eprintln!("{}", json!({"event": "config.warning", "msg": w}));
            }
            // The report runs on an INVALID config on purpose, so it has to
            // say so — a document printed without its errors would read as a
            // clean bill of health.
            let diags = agentd::config::v2::validate(&loaded);
            for e in &diags.errors {
                eprintln!("{}", json!({"event": "config.invalid", "error": e}));
            }
            let files: Vec<String> = loaded.files.iter().map(|(p, _)| p.clone()).collect();
            println!(
                "{}",
                serde_json::to_string_pretty(&agentd::config::effective::report(
                    &loaded.trace,
                    &files,
                    &loaded.settings.agent.document_config,
                ))
                .unwrap_or_else(|_| "{}".to_string())
            );
            exit::SUCCESS
        }
        Ask::Capabilities => {
            println!(
                "{}",
                serde_json::to_string_pretty(&agentd::runtime::capabilities(&loaded))
                    .unwrap_or_else(|_| "{}".to_string())
            );
            exit::SUCCESS
        }
        // `--login <target>`: the interactive OAuth device flow.
        Ask::Login(target) => {
            #[cfg(feature = "oauth")]
            {
                match agentd::auth::login::run_cli(
                    &loaded.settings,
                    &target,
                    std::time::Duration::from_secs(30),
                ) {
                    Ok(msg) => {
                        println!("{msg}");
                        exit::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("agentd: login failed: {e}");
                        exit::USAGE
                    }
                }
            }
            #[cfg(not(feature = "oauth"))]
            {
                let _ = &target;
                eprintln!("agentd: --login requires building with --features oauth");
                exit::USAGE
            }
        }
        // `--logout <target>`: evict a cached credential (no feature needed).
        Ask::Logout(target) => {
            // A server that references a catalog entry caches its credential
            // under `service:<entry>`, not `mcp:<name>` — canonicalize first, or
            // logout would report success while leaving the real key in place.
            let target = agentd::auth::canonical_target(&loaded.settings, &target);
            let dir = agentd::auth::cache::default_dir();
            match agentd::auth::cache::evict_file(&dir, &target) {
                Ok(()) => {
                    println!("logged out of {target}");
                    exit::SUCCESS
                }
                Err(e) => {
                    eprintln!("agentd: logout failed: {e}");
                    exit::USAGE
                }
            }
        }
        Ask::Run => {
            // Record what shaped the durable state before the runtime opens the
            // store. `restore()` compares this with the digest the manifest
            // carries and *reports* a difference — it never gates on it.
            // Durable identity is `agent.name`; keying it on a config hash
            // would orphan a live workflow on the most ordinary edit.
            agentd::state::record_config_digest(&loaded.settings);
            agentd::runtime::run(&loaded, args, env)
        }
    }
}

/// Load a daemon invocation the one way both the supervisor and the `agentd
/// tui|ui` launcher do: consume the per-process intents, apply `--env` files,
/// refuse the flat schema, then load and validate. `Err` carries the exit code,
/// its diagnostic already printed.
pub(crate) fn load_invocation(
    args: &[String],
    env: &[(String, String)],
) -> Result<Invocation, i32> {
    use agentd::config::v2::{self, Detected};
    // `--fresh` is an intent for *this* process's life, not a
    // setting: it has no document path, and a file or env var that pinned an
    // instance to never resuming would be a footgun. So it is consumed here,
    // before the settings model ever sees the argv (which would reject it as an
    // unknown argument), and recorded where `state::Durable::restore` reads it.
    let fresh = args.iter().any(|a| a == "--fresh");
    // `--prompt-missing` is the same kind of flag: an intent for THIS process's
    // life (ask me for the secrets the preflight finds missing), not a setting
    // a file could pin. Consumed here, recorded where the startup preflight
    // reads it.
    let prompt_missing = args.iter().any(|a| a == "--prompt-missing");
    // `--env <FILE>` (repeatable): load dotenv files into THIS process's
    // environment before anything reads it. Same family again — an input to
    // this invocation, not a setting. Applied with real-environment-wins (a
    // deployment override beats the checked-in file), then the layered config
    // env is rebuilt so `AGENTD_*` keys from a file work like any other.
    let mut env_files: Vec<String> = Vec::new();
    {
        let mut it = args.iter();
        while let Some(a) = it.next() {
            if let Some(v) = a.strip_prefix("--env=") {
                env_files.push(v.to_string());
            } else if a == "--env" {
                match it.next() {
                    Some(v) => env_files.push(v.clone()),
                    None => {
                        eprintln!("agentd: --env needs a file path");
                        return Err(exit::USAGE);
                    }
                }
            }
        }
    }
    let mut args2: Vec<String> = Vec::new();
    {
        let mut skip = false;
        for a in args {
            if skip {
                skip = false;
                continue;
            }
            if a == "--env" {
                skip = true;
                continue;
            }
            if a == "--fresh" || a == "--prompt-missing" || a.starts_with("--env=") {
                continue;
            }
            args2.push(a.clone());
        }
    }
    let args = args2.as_slice();
    let env: Vec<(String, String)> = if env_files.is_empty() {
        env.to_vec()
    } else {
        match agentd::config::envfile::load_files(&env_files) {
            Ok(pairs) => {
                for (k, v) in pairs {
                    if std::env::var_os(&k).is_none() {
                        // Single-threaded here — before signals, threads, or
                        // any config read — which is what makes set_var sound.
                        unsafe { std::env::set_var(&k, &v) };
                    }
                }
                std::env::vars().collect()
            }
            Err(e) => {
                eprintln!("agentd: {e}");
                return Err(exit::USAGE);
            }
        }
    };
    let env = env.as_slice();
    if fresh {
        agentd::state::request_fresh();
    }
    if prompt_missing {
        agentd::config::prompt::request_prompt_missing();
    }
    let detected = match v2::probe(args, env) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("{e}");
            return Err(exit::USAGE);
        }
    };
    if detected == Detected::V1 {
        eprintln!(
            "agentd: this configuration uses the flat schema, which agentd does not accept. \
Migrate to `config_version: \"1\"` with sections (agent / intelligence / a2a / workflows); \
see docs/configuration.md."
        );
        return Err(exit::USAGE);
    }
    let (loaded, ask) = match v2::load(args, env) {
        Ok(x) => x,
        Err(ConfigError::Validate(Ok(line))) => {
            eprintln!("{line}");
            return Err(exit::SUCCESS);
        }
        Err(ConfigError::Validate(Err(lines))) => {
            eprintln!("{lines}");
            return Err(exit::USAGE);
        }
        Err(ConfigError::Usage(s)) => {
            eprintln!("{s}");
            return Err(exit::USAGE);
        }
        Err(other) => {
            eprintln!("{other:?}");
            return Err(exit::USAGE);
        }
    };
    Ok(Invocation {
        loaded,
        ask,
        args: args.to_vec(),
        env: env.to_vec(),
    })
}
