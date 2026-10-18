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
    std::process::exit(dispatch());
}

/// Picks the role this process plays — launcher, hidden mock, subagent
/// re-exec or the supervisor — from argv and the environment alone.
fn dispatch() -> i32 {
    // Before any role opens anything: a descriptor this process inherited
    // without close-on-exec would reach every child the launcher, the daemon
    // or a subagent spawns.
    agentd::signals::cloexec_inherited_fds();
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

    // Hidden built-in Streamable HTTP mock MCP server (tests/dev). Binds
    // loopback TCP (127.0.0.1:0) and announces the bound address through
    // <addr-file>.
    #[cfg(any(feature = "internal-mocks", debug_assertions))]
    if argv.get(1).map(String::as_str) == Some("--internal-mock-mcp-http") {
        return mock_mcp_http(&argv[2..]);
    }

    // Hidden fake Company (tests/conformance): a Company as the Personal
    // Agent Protocol draft describes one, over HTTPS on loopback, announced
    // through <addr-file>.
    #[cfg(all(
        feature = "oauth",
        feature = "tls",
        any(feature = "internal-mocks", debug_assertions)
    ))]
    if argv.get(1).map(String::as_str) == Some("--internal-mock-company") {
        return mock_company(&argv[2..]);
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
    run(&argv[1..], &env)
}

/// `--internal-mock-mcp-http <addr-file> <uri> [--no-emit]
/// [--protocol-version <v>] [--events <extension|top-level|none>]
/// [--hook-ca <pem>] [--listen <addr>]`: the mock
/// at 2025-11-25 (the `initialize` handshake; the default) or 2026-07-28
/// (`server/discover`), its MCP Events declared in SEP-3415's spelling (the
/// default), the design sketch's, or not at all; its webhook deliveries
/// trust the CA in `--hook-ca`. Anything else is a usage error.
#[cfg(any(feature = "internal-mocks", debug_assertions))]
fn mock_mcp_http(args: &[String]) -> i32 {
    use agentd::mcp::mock_http::{Lifecycle, Options, run_with};
    let mut positional = Vec::new();
    let mut opts = Options::new("mock://resource");
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--no-emit" => opts.emit = false,
            "--protocol-version" => {
                let asked = it.next().map(String::as_str).unwrap_or_default();
                let Some(lifecycle) = Lifecycle::of_revision(asked) else {
                    eprintln!(
                        "--internal-mock-mcp-http: --protocol-version {asked:?}: the mock speaks 2026-07-28 and 2025-11-25"
                    );
                    return exit::USAGE;
                };
                opts.lifecycle = lifecycle;
            }
            "--events" => {
                use agentd::mcp::mock_events::Spelling;
                opts.events = match it.next().map(String::as_str) {
                    Some("extension") => Spelling::Extension,
                    Some("top-level") => Spelling::TopLevel,
                    Some("none") => Spelling::None,
                    other => {
                        eprintln!(
                            "--internal-mock-mcp-http: --events {other:?}: extension, top-level or none"
                        );
                        return exit::USAGE;
                    }
                };
            }
            "--hook-ca" => opts.hook_ca = it.next().cloned(),
            "--listen" => opts.listen = it.next().cloned(),
            _ => positional.push(arg.as_str()),
        }
    }
    let addr_file = positional
        .first()
        .copied()
        .unwrap_or("/tmp/agentd-mock-mcp.addr");
    if let Some(uri) = positional.get(1) {
        opts.uri = (*uri).to_string();
    }
    run_with(addr_file, &opts)
}

/// `--internal-mock-company <addr-file> --cert <pem> --key <pem> [--ca <pem>]
/// [--variant <name>]… [--listen <addr>]`: the fake Company, serving HTTPS
/// with the identity in `--cert` and `--key`, trusting `--ca` when it
/// fetches a Personal Agent's documents, and departing from the draft as
/// each `--variant` names. Anything else is a usage error.
#[cfg(all(
    feature = "oauth",
    feature = "tls",
    any(feature = "internal-mocks", debug_assertions)
))]
fn mock_company(args: &[String]) -> i32 {
    use agentd::companies::mock::{Options, Variant, run_with};
    let usage = |what: &str| {
        eprintln!("--internal-mock-company: {what}");
        exit::USAGE
    };
    let mut opts = Options::default();
    let mut addr_file = None;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        let value = match arg.as_str() {
            "--cert" | "--key" | "--ca" | "--variant" | "--listen" => match it.next() {
                Some(v) => v.clone(),
                None => return usage(&format!("{arg} needs a value")),
            },
            other if addr_file.is_none() && !other.starts_with("--") => {
                addr_file = Some(other.to_string());
                continue;
            }
            other => return usage(&format!("unknown argument {other:?}")),
        };
        match arg.as_str() {
            "--cert" => opts.cert = value,
            "--key" => opts.key = value,
            "--ca" => opts.ca = Some(value),
            "--listen" => opts.listen = Some(value),
            _ => match Variant::parse(&value) {
                Some(v) => opts.variants.push(v),
                None => {
                    let names: Vec<&str> = Variant::ALL.iter().map(|v| v.name()).collect();
                    return usage(&format!("--variant {value:?}: one of {}", names.join(", ")));
                }
            },
        }
    }
    let Some(addr_file) = addr_file else {
        return usage("an <addr-file> is required");
    };
    if opts.cert.is_empty() || opts.key.is_empty() {
        return usage("--cert and --key are required: the fake Company serves HTTPS only");
    }
    run_with(&addr_file, &opts)
}

/// A loaded daemon invocation: the configuration, what was asked of it, and
/// the arguments and environment it was loaded from — the per-process intents
/// (`--fresh`, `--prompt-missing`, `--env`) already consumed.
pub(crate) struct Invocation {
    pub loaded: agentd::config::settings::Loaded,
    pub ask: agentd::config::settings::Ask,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
}

/// The agentd supervisor: load + validate the configuration and run it (or
/// answer an early-exit ask). `settings::load` emits the precise per-key
/// diagnostics that say which entries are unrecognized.
fn run(args: &[String], env: &[(String, String)]) -> i32 {
    use agentd::config::settings::{self, Ask};
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
                settings::help_text().replace(
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
                serde_json::to_string_pretty(&settings::schema::schema())
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
        // What agentd.dev publishes next to each extension URI, printed from
        // the values the listener checks against: `scripts/gen-schemas.sh`
        // writes the site's copies from these, and CI diffs them.
        Ask::ExtensionSchema(name) => {
            use agentd::runtime::surface::{Ext, bundle_of};
            match bundle_of(&name) {
                Some(bundle) => {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&bundle).unwrap_or_else(|_| "{}".to_string())
                    );
                    exit::SUCCESS
                }
                None => {
                    let known: Vec<&str> = Ext::ALL.iter().map(|e| e.name()).collect();
                    eprintln!(
                        "agentd: no extension is called {name:?}; the extensions with a schema are {}",
                        known.join(", ")
                    );
                    exit::USAGE
                }
            }
        }
        Ask::Extensions => {
            println!(
                "{}",
                serde_json::to_string_pretty(&agentd::runtime::surface::registry())
                    .unwrap_or_else(|_| "[]".to_string())
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
                json!({"event": "config.valid",
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
            let diags = agentd::config::settings::validate(&loaded);
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
        // `--print-company-client <dir>`: the client identity for static
        // hosting. The key is created when absent; nothing is dialled.
        Ask::PrintCompanyClient(dir) => {
            #[cfg(feature = "companies")]
            {
                match agentd::companies::print_client(&loaded.settings, std::path::Path::new(&dir))
                {
                    Ok(files) => {
                        for f in files {
                            println!("{}", f.display());
                        }
                        exit::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("agentd: --print-company-client: {e}");
                        exit::USAGE
                    }
                }
            }
            #[cfg(not(feature = "companies"))]
            {
                let _ = &dir;
                eprintln!(
                    "agentd: --print-company-client needs the companies feature (build with --features companies)"
                );
                exit::USAGE
            }
        }
        Ask::Capabilities => {
            println!(
                "{}",
                serde_json::to_string_pretty(&agentd::runtime::capabilities(&loaded))
                    .unwrap_or_else(|_| "{}".to_string())
            );
            exit::SUCCESS
        }
        // `--login company:<name>`: the running daemon asks the User to sign
        // in at the Company; this prints where, and how it ended.
        Ask::Login(target) if target.starts_with("company:") => {
            company_cli(&loaded.settings, &target, true)
        }
        Ask::Logout(target) if target.starts_with("company:") => {
            company_cli(&loaded.settings, &target, false)
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
        // `--logout <target>`: evict a cached credential (no feature needed),
        // then revoke it where it was issued (RFC 7009, with `oauth`).
        Ask::Logout(target) => {
            let dir = agentd::auth::cache::default_dir();
            #[cfg(feature = "oauth")]
            let out = agentd::auth::revoke::logout(
                &loaded.settings,
                &target,
                &dir,
                std::time::Duration::from_secs(30),
            );
            // A server that references a catalog entry caches its credential
            // under `service:<entry>`, not `mcp:<name>` — canonicalize first, or
            // logout would report success while leaving the real key in place.
            #[cfg(not(feature = "oauth"))]
            let out = {
                let target = agentd::auth::canonical_target(&loaded.settings, &target);
                agentd::auth::cache::take_file(&dir, &target, std::time::Duration::from_secs(30))
                    .map(|_| format!("logged out of {target}"))
            };
            match out {
                Ok(msg) => {
                    println!("{msg}");
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

/// `--login` / `--logout company:<name>` (`login`): a thin client of the
/// running daemon, which does the sign-in or sign-out itself.
fn company_cli(settings: &agentd::config::settings::Settings, target: &str, login: bool) -> i32 {
    let name = target.trim_start_matches("company:");
    #[cfg(all(feature = "companies", feature = "a2a"))]
    {
        let out = if login {
            agentd::companies::cli::login(settings, name, &mut std::io::stdout())
        } else {
            agentd::companies::cli::logout(settings, name, &mut std::io::stdout())
        };
        match out {
            Ok(_) => exit::SUCCESS,
            Err(agentd::companies::cli::Failed::Usage(e)) => {
                eprintln!("agentd: {target}: {e}");
                exit::USAGE
            }
            // What the daemon said last is already on stdout.
            Err(agentd::companies::cli::Failed::Ended(e)) => {
                if !e.is_empty() {
                    eprintln!("agentd: {target}: {e}");
                }
                exit::GENERIC
            }
        }
    }
    #[cfg(not(all(feature = "companies", feature = "a2a")))]
    {
        let _ = (settings, name, login);
        eprintln!(
            "agentd: {target} needs the companies and a2a features (build with --features companies,a2a)"
        );
        exit::USAGE
    }
}

/// Load a daemon invocation the one way both the supervisor and the `agentd
/// tui|ui` launcher do: consume the per-process intents, apply `--env` files,
/// then load and validate. `Err` carries the exit code,
/// its diagnostic already printed.
pub(crate) fn load_invocation(
    args: &[String],
    env: &[(String, String)],
) -> Result<Invocation, i32> {
    use agentd::config::settings;
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
    let (loaded, ask) = match settings::load(args, env) {
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
    };
    Ok(Invocation {
        loaded,
        ask,
        args: args.to_vec(),
        env: env.to_vec(),
    })
}
