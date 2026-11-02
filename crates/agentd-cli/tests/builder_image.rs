// SPDX-License-Identifier: AGPL-3.0-only
//! **The builder image offers what agentd has, and the release goes through
//! it.**
//!
//! `ghcr.io/agentd-dev/agentd-builder` is the source of one release with
//! every dependency vendored, cargo-deny and `agentd-build`, which builds
//! agentd with the features someone asks for — out of an allow-list,
//! `packaging/builder/features`. The official image is built by the same
//! `agentd-build`. Each piece is written once and copied nowhere, so these
//! hold the copies that have to exist to the thing they copy:
//!
//! - the features file names every agentd-cli feature exactly once, its
//!   `release` tier is release.yml's FEATURES, its `default` tier the
//!   manifest's defaults, a `tested` feature has a CI row of its own, `exec`
//!   asks to be confirmed and `internal-mocks` is refused;
//! - the Dockerfile's official build runs `agentd-build` with FEATURES, and
//!   the image it ships is labelled with what `agentd --build-info` reports
//!   for them;
//! - the builder's toolchain is pinned by digest and its cargo-deny is CI's;
//! - the builder records the release and commit it was told, and nothing
//!   when it was told none;
//! - release.yml publishes the builder with the image job's tags, signature
//!   and SBOM attestation, builds the image FROM it, and publishes nothing
//!   before ci.yml's deny job passes, and nothing to crates.io before the
//!   builder and the image are out; ci.yml's image job exercises it;
//! - and `agentd-build` itself, driven here with a cargo that only records
//!   what it was asked, refuses before it compiles, warns outside the tested
//!   sets, judges the build's own graph and writes what it says it writes.
#![cfg(unix)]

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn read(rel: &str) -> String {
    let p = root().join(rel);
    std::fs::read_to_string(&p).unwrap_or_else(|e| panic!("{}: {e}", p.display()))
}

/// `FEATURES: "a,b,c"` from release.yml.
fn release_features() -> Vec<String> {
    let text = read(".github/workflows/release.yml");
    let line = text
        .lines()
        .find(|l| l.trim_start().starts_with("FEATURES:"))
        .expect("release.yml declares FEATURES");
    let list = line.split_once(':').unwrap().1.trim().trim_matches('"');
    list.split(',').map(|s| s.trim().to_string()).collect()
}

/// agentd-cli's `[features]`: each name with the features of agentd-cli it
/// turns on (the `agentd/…` it forwards aside). `default` is in it.
fn feature_graph() -> BTreeMap<String, Vec<String>> {
    let mut out = BTreeMap::new();
    let mut inside = false;
    for line in read("crates/agentd-cli/Cargo.toml").lines() {
        let t = line.trim();
        if t.starts_with('[') && !t.starts_with("[\"") {
            inside = t == "[features]";
            continue;
        }
        if !inside || t.starts_with('#') {
            continue;
        }
        let Some((name, deps)) = t.split_once('=') else {
            continue;
        };
        let deps = deps
            .trim()
            .trim_start_matches('[')
            .trim_end_matches(']')
            .split(',')
            .map(|d| d.trim().trim_matches('"').to_string())
            .filter(|d| !d.is_empty() && !d.contains('/') && !d.contains(':'))
            .collect();
        out.insert(name.trim().to_string(), deps);
    }
    assert!(out.len() > 10, "[features] parsed as {out:?}");
    out
}

/// `features` with every feature they turn on, `default` expanded: what
/// `agentd --build-info` lists for a build of them.
fn closure(features: &[String]) -> Vec<String> {
    let graph = feature_graph();
    let mut on: BTreeSet<String> = features.iter().cloned().collect();
    loop {
        let more: Vec<String> = on
            .iter()
            .flat_map(|f| graph.get(f).cloned().unwrap_or_default())
            .filter(|d| !on.contains(d))
            .collect();
        if more.is_empty() {
            break;
        }
        on.extend(more);
    }
    on.remove("default");
    on.into_iter().collect()
}

/// The features file: (name, tier, reason) per line, comments and blank lines
/// aside.
fn features_file() -> Vec<(String, String, String)> {
    read("packaging/builder/features")
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.trim_start().starts_with('#'))
        .map(|l| {
            let mut parts = l.splitn(3, '\t');
            let (Some(n), Some(t), Some(r)) = (parts.next(), parts.next(), parts.next()) else {
                panic!("packaging/builder/features: not name<TAB>tier<TAB>reason: {l:?}");
            };
            (n.to_string(), t.to_string(), r.trim().to_string())
        })
        .collect()
}

fn tier(t: &str) -> BTreeSet<String> {
    features_file()
        .into_iter()
        .filter(|(_, tier, _)| tier == t)
        .map(|(n, _, _)| n)
        .collect()
}

/// The rows of ci.yml's `features` matrix, as written.
fn ci_rows() -> Vec<String> {
    let ci = read(".github/workflows/ci.yml");
    let matrix = ci
        .split("\n        features:\n")
        .nth(1)
        .expect("ci.yml has a features matrix");
    matrix
        .lines()
        .map_while(|l| l.trim().strip_prefix("- \""))
        .map(|l| l.split_once('"').unwrap().0.to_string())
        .collect()
}

#[test]
fn the_features_file_names_every_feature_once() {
    let file = features_file();
    let mut named: Vec<&str> = file.iter().map(|(n, _, _)| n.as_str()).collect();
    named.sort_unstable();
    let before = named.len();
    named.dedup();
    assert_eq!(before, named.len(), "a feature is listed twice");
    let declared: Vec<String> = feature_graph()
        .into_keys()
        .filter(|f| f != "default")
        .collect();
    assert_eq!(
        named, declared,
        "packaging/builder/features must name every agentd-cli feature, `default` aside"
    );
    for (name, tier, reason) in &file {
        assert!(
            ["default", "release", "tested", "confirm", "refused"].contains(&tier.as_str()),
            "{name}: {tier:?} is not a tier"
        );
        assert!(
            !reason.is_empty(),
            "{name}: a feature's line says why it is in its tier"
        );
    }
}

#[test]
fn the_tiers_are_what_the_release_and_ci_say() {
    let release: BTreeSet<String> = release_features().into_iter().collect();
    assert_eq!(
        tier("release"),
        release,
        "the features file's release tier must be release.yml's FEATURES"
    );
    let default: BTreeSet<String> = feature_graph()["default"].iter().cloned().collect();
    assert_eq!(
        tier("default"),
        default,
        "the default tier is agentd-cli's default features"
    );
    // A tested feature is one CI builds and tests on its own.
    let rows = ci_rows();
    for f in tier("tested") {
        assert!(
            rows.iter().any(|r| r == &format!("--features {f}")),
            "{f} is tiered tested, and ci.yml has no row `--features {f}`"
        );
    }
    assert_eq!(
        tier("confirm"),
        BTreeSet::from(["exec".to_string()]),
        "exec runs local commands: it is built only when asked twice"
    );
    assert_eq!(
        tier("refused"),
        BTreeSet::from(["internal-mocks".to_string()]),
        "internal-mocks is test scaffolding: no builder builds it"
    );
}

/// The Dockerfile's stages as (the `FROM` line, the stage's lines).
fn stages() -> Vec<(String, String)> {
    let dockerfile = read("Dockerfile");
    let mut out: Vec<(String, String)> = Vec::new();
    for line in dockerfile.lines() {
        if line.starts_with("FROM ") {
            out.push((line.to_string(), String::new()));
        } else if let Some((_, body)) = out.last_mut() {
            body.push_str(line);
            body.push('\n');
        }
    }
    out
}

fn stage(named: &str) -> String {
    stages()
        .into_iter()
        .find(|(from, _)| from.ends_with(&format!(" AS {named}")))
        .unwrap_or_else(|| panic!("the Dockerfile has no stage {named}"))
        .1
}

/// The default value of a global `ARG`.
fn dockerfile_arg(name: &str) -> String {
    read("Dockerfile")
        .lines()
        .find_map(|l| l.strip_prefix(&format!("ARG {name}=")))
        .unwrap_or_else(|| panic!("the Dockerfile declares no ARG {name}=…"))
        .trim()
        .trim_matches('"')
        .to_string()
}

#[test]
fn the_official_image_is_built_by_agentd_build() {
    let names: Vec<String> = stages().into_iter().map(|(from, _)| from).collect();
    let at = |s: &str| names.iter().position(|n| n.ends_with(s));
    let (Some(toolchain), Some(builder), Some(build)) =
        (at(" AS toolchain"), at(" AS builder"), at(" AS build"))
    else {
        panic!("the Dockerfile's stages are {names:?}");
    };
    assert!(
        toolchain < builder && builder < build && build + 1 == names.len() - 1,
        "the stages run toolchain, builder, build, then the runtime image last: {names:?}"
    );
    assert!(
        names[builder].starts_with("FROM toolchain ") && names[build] == "FROM builder AS build",
        "the builder is FROM the toolchain and the official build FROM the builder: {names:?}"
    );
    // The runtime stage is the last, so a plain `docker build .` makes it.
    assert_eq!(names.last().unwrap(), "FROM scratch");

    let build = stage("build");
    let run: Vec<&str> = build.lines().filter(|l| l.starts_with("RUN ")).collect();
    assert_eq!(
        run,
        [
            r#"RUN agentd-build --features "$FEATURES" --expect-features "$BUILD_INFO_FEATURES" --out /out"#
        ],
        "the official build is agentd-build with FEATURES and nothing else"
    );
    let runtime = stages().pop().unwrap().1;
    assert!(
        runtime
            .lines()
            .any(|l| l == "COPY --from=build /out/agentd /agentd"),
        "the image ships the binary agentd-build wrote"
    );

    // The builder pre-warms its target directory with the very build the
    // official stage asks for — only `--out` differs — so that stage finds
    // it current instead of compiling the release a second time.
    let args = |line: &str| -> String {
        let line = line.trim_start_matches("RUN ");
        line.split(" --out ").next().unwrap().to_string()
    };
    let builder = stage("builder");
    let prewarm: Vec<&str> = builder
        .lines()
        .filter(|l| l.starts_with("RUN agentd-build "))
        .collect();
    assert_eq!(prewarm.len(), 1, "the builder pre-warms once: {prewarm:?}");
    assert_eq!(args(prewarm[0]), args(run[0]));
    assert!(
        builder.contains("ENV CARGO_TARGET_DIR=/var/cache/agentd-build/target\n"),
        "the pre-warmed target directory is the one agentd-build uses"
    );
}

/// `dev.agentd.features` says what `agentd --build-info` says. FEATURES
/// cannot: a label is not computed, and the binary also has the default `tls`
/// and what each feature implies. So the image is labelled with
/// BUILD_INFO_FEATURES, the official build refuses unless it is exactly what
/// the binary reports (`--expect-features`), and its default is held here to
/// the release set's closure.
/// The licence and the third-party notices travel with every binary the
/// release ships: NOTICE carries the CC BY 4.0 attribution for the
/// instruction.md registries each binary embeds as data, so a tarball or an
/// image without it redistributes them unattributed.
#[test]
fn the_licence_and_notices_ship_with_every_binary() {
    for f in ["LICENSE", "NOTICE"] {
        assert!(root().join(f).is_file(), "the repository has no {f}");
        assert!(
            !read(".dockerignore").lines().any(|l| l.trim() == f),
            ".dockerignore keeps {f} out of the build context"
        );
    }
    // The image: from the build stage, the source the binary was built from.
    let builder = stage("builder");
    assert!(
        builder.lines().any(|l| l == "WORKDIR /usr/src/agentd")
            && builder.lines().any(|l| l == "COPY . ."),
        "the builder holds the source at /usr/src/agentd"
    );
    let runtime = stages().pop().unwrap().1;
    assert!(
        runtime.lines().any(|l| l
            == "COPY --from=build /usr/src/agentd/LICENSE /usr/src/agentd/NOTICE /usr/share/doc/agentd/"),
        "the image ships LICENSE and NOTICE under /usr/share/doc/agentd/"
    );
    // Each tarball: `agentd`, `LICENSE` and `NOTICE`, side by side.
    let binaries = job(&read(".github/workflows/release.yml"), "binaries");
    let tar: Vec<&str> = binaries
        .lines()
        .map(str::trim)
        .filter(|l| l.starts_with("tar czf "))
        .collect();
    assert_eq!(tar.len(), 1, "the binaries job packs one tarball: {tar:?}");
    assert!(
        tar[0].ends_with(".tar.gz\" agentd LICENSE NOTICE"),
        "each tarball holds agentd, LICENSE and NOTICE: {}",
        tar[0]
    );
    assert!(
        binaries
            .lines()
            .any(|l| l.trim().starts_with("cp ") && l.ends_with(" LICENSE NOTICE dist/")),
        "the binaries job copies LICENSE and NOTICE beside the binary"
    );
}

#[test]
fn the_image_is_labelled_with_its_features() {
    let runtime = stages().pop().unwrap().1;
    assert!(
        runtime.contains(r#"dev.agentd.features="${BUILD_INFO_FEATURES}""#),
        "the runtime stage labels dev.agentd.features with BUILD_INFO_FEATURES"
    );
    assert!(runtime.lines().any(|l| l == "ARG BUILD_INFO_FEATURES"));
    let mut release = release_features();
    release.push("default".into());
    assert_eq!(
        dockerfile_arg("BUILD_INFO_FEATURES"),
        closure(&release).join(","),
        "BUILD_INFO_FEATURES must be what --build-info reports for release.yml's FEATURES"
    );
    let builder = stage("builder");
    assert!(
        builder.contains(r#"dev.agentd.builder.release-features="${FEATURES}""#),
        "the builder image names the release set it was made for"
    );
}

#[test]
fn the_builder_is_pinned_and_judges_with_cis_cargo_deny() {
    let manifest = read("crates/agentd-cli/Cargo.toml");
    let rust_version = manifest
        .lines()
        .find_map(|l| l.strip_prefix("rust-version = "))
        .unwrap()
        .trim_matches('"');
    let from = stages()
        .into_iter()
        .map(|(from, _)| from)
        .find(|f| f.ends_with(" AS toolchain"))
        .unwrap();
    let image = from
        .strip_prefix("FROM ")
        .and_then(|f| f.strip_suffix(" AS toolchain"))
        .unwrap();
    let (tag, digest) = image
        .split_once("@sha256:")
        .unwrap_or_else(|| panic!("the toolchain is not pinned by digest: {from}"));
    assert_eq!(
        tag,
        format!("rust:{rust_version}-alpine"),
        "the toolchain is the crates' rust-version"
    );
    assert!(
        digest.len() == 64 && digest.bytes().all(|b| b.is_ascii_hexdigit()),
        "a digest is 64 hex digits: {digest}"
    );
    assert!(
        read("Dockerfile").contains(&format!(
            r#"org.opencontainers.image.base.digest="sha256:{digest}""#
        )),
        "the builder's base.digest label is the digest it is FROM"
    );

    let ci = read(".github/workflows/ci.yml");
    let pin = |key: &str| {
        ci.lines()
            .find_map(|l| l.trim().strip_prefix(&format!("{key}: ")))
            .unwrap_or_else(|| panic!("ci.yml's deny job has no {key}"))
            .trim_matches('"')
            .to_string()
    };
    assert_eq!(
        dockerfile_arg("CARGO_DENY_VERSION"),
        pin("CARGO_DENY_VERSION")
    );
    assert_eq!(
        dockerfile_arg("CARGO_DENY_SHA256_AMD64"),
        pin("CARGO_DENY_SHA256"),
        "the builder's x86_64 cargo-deny is the binary CI verifies"
    );
    let arm = dockerfile_arg("CARGO_DENY_SHA256_ARM64");
    assert!(arm.len() == 64 && arm.bytes().all(|b| b.is_ascii_hexdigit()));
    let deny = stage("cargo-deny");
    assert!(
        deny.contains(r#"sha256sum -c -"#),
        "the cargo-deny binary is checked against its checksum"
    );
}

/// One job of a workflow: its lines after its key, up to the first line
/// indented less than a job's body (the next job, or the comment above it).
fn job(workflow: &str, name: &str) -> String {
    let body = workflow
        .split(&format!("\n  {name}:\n"))
        .nth(1)
        .unwrap_or_else(|| panic!("no job {name}"));
    body.lines()
        .take_while(|l| l.trim().is_empty() || l.starts_with("    "))
        .map(|l| format!("{l}\n"))
        .collect()
}

#[test]
fn the_release_publishes_the_builder_like_the_image() {
    let release = read(".github/workflows/release.yml");
    let image = &job(&release, "container");
    let builder = &job(&release, "builder");

    // The image job builds the default target: the official image.
    assert!(
        !image.contains("target:"),
        "the image job builds the default target"
    );
    assert!(builder.contains("          target: builder\n"));
    assert!(builder.contains("images: ghcr.io/${{ github.repository_owner }}/agentd-builder\n"));
    assert!(builder.contains("          platforms: linux/amd64,linux/arm64\n"));
    assert!(builder.contains("          push: true\n"));
    for needed in [
        "packages: write",
        "id-token: write",
        "FEATURES=${{ env.FEATURES }}",
    ] {
        assert!(builder.contains(needed), "the builder job lacks {needed}");
    }

    // The image is built FROM the builder this job pushed, by digest: it
    // takes the place of the Dockerfile's `builder` stage, so the image's
    // binary is the release set compiled in the published builder, and not
    // a second compile of the stage on another runner.
    assert!(
        builder.contains("    outputs:\n      digest: ${{ steps.build.outputs.digest }}\n"),
        "the builder job outputs the digest it pushed"
    );
    assert!(
        image.contains("    needs: [builder]\n"),
        "the image job waits for the builder"
    );
    assert!(
        image.contains(
            "          build-contexts: |\n            builder=docker-image://ghcr.io/${{ github.repository_owner }}/agentd-builder@${{ needs.builder.outputs.digest }}\n"
        ),
        "the image job builds FROM the published builder, by digest"
    );

    // The same tags, from the same metadata-action block.
    let tags = |job: &str| -> Vec<String> {
        let block = job
            .split("          tags: |\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no metadata-action tags in:\n{job}"));
        block
            .lines()
            .take_while(|l| l.starts_with("            "))
            .map(|l| l.trim().to_string())
            .collect()
    };
    assert!(
        tags(image).len() >= 2,
        "the image's tags parsed as {:?}",
        tags(image)
    );
    assert_eq!(
        tags(builder),
        tags(image),
        "the builder is tagged as the image is"
    );

    // The same signature and SBOM attestation, step for step, of its own digest.
    let signing = |job: &str| {
        job.split("      - uses: sigstore/cosign-installer@v3\n")
            .nth(1)
            .unwrap_or_else(|| panic!("no cosign steps in:\n{job}"))
            .trim_end()
            .to_string()
    };
    assert_eq!(
        signing(builder),
        signing(image).replace("/agentd@", "/agentd-builder@"),
        "the builder must be cosign-signed and SBOM-attested exactly as the image is"
    );
    assert!(signing(builder).contains("cosign sign --yes"));
    assert!(signing(builder).contains("cosign attest --yes --predicate sbom.spdx.json"));
}

/// A job's `needs`, as written.
fn needs(job: &str) -> Vec<String> {
    job.lines()
        .find_map(|l| l.strip_prefix("    needs: "))
        .map(|n| {
            n.trim_matches(['[', ']'])
                .split(',')
                .map(|s| s.trim().to_string())
                .collect()
        })
        .unwrap_or_default()
}

/// A release publishes nothing its supply-chain gate refuses. The builder's
/// `agentd-build` judges the release graph against the advisory database as
/// it is when it builds, so an advisory published since CI last ran refuses
/// the builder and the image. ci.yml's deny job runs first, line for line,
/// and every other job waits for it; crates.io, the one step a new tag
/// cannot undo, also waits for the binaries, the builder and the image.
#[test]
fn a_release_publishes_nothing_its_supply_chain_gate_refuses() {
    let release = read(".github/workflows/release.yml");
    let ci = read(".github/workflows/ci.yml");
    assert_eq!(
        job(&release, "deny").trim_end(),
        job(&ci, "deny").trim_end(),
        "release.yml's deny job is ci.yml's, line for line"
    );

    let jobs: Vec<String> = release
        .split("\njobs:\n")
        .nth(1)
        .expect("release.yml has jobs")
        .lines()
        .filter_map(|l| l.strip_prefix("  ")?.strip_suffix(':'))
        .filter(|n| n.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
        .map(str::to_string)
        .collect();
    assert!(
        jobs.len() >= 6 && jobs.iter().any(|j| j == "deny"),
        "release.yml's jobs parsed as {jobs:?}"
    );
    // Every job a job waits for, through the jobs it waits for.
    let after = |name: &str| -> BTreeSet<String> {
        let mut seen = BTreeSet::new();
        let mut todo = vec![name.to_string()];
        while let Some(j) = todo.pop() {
            for n in needs(&job(&release, &j)) {
                assert!(jobs.contains(&n), "{j} needs {n}, which is not a job");
                if seen.insert(n.clone()) {
                    todo.push(n);
                }
            }
        }
        seen
    };
    for j in jobs.iter().filter(|j| *j != "deny") {
        assert!(
            after(j).contains("deny"),
            "release.yml's {j} job does not wait for the deny job"
        );
    }
    let crates = after("crates");
    for artifact in ["binaries", "builder", "container"] {
        assert!(
            crates.contains(artifact),
            "the crates.io publish does not wait for the {artifact} job"
        );
    }
}

#[test]
fn ci_exercises_the_builder_without_a_second_release_build() {
    let ci = read(".github/workflows/ci.yml");
    let image = &job(&ci, "image");
    assert!(
        image.contains("          target: builder\n"),
        "ci.yml's image job builds the builder"
    );
    for needed in [
        "docker run --rm agentd-builder:ci --list\n",
        "refused --features exec\n",
        "refused --features \"$unknown\"\n",
        "docker run --rm agentd:ci --build-info\n",
        "--intelligence mock:final",
        "dev.agentd.features",
    ] {
        assert!(
            image.contains(needed),
            "ci.yml's image job does not run {needed:?}"
        );
    }
    // One release build: the builder stage pre-warms it, and the official
    // build reuses the builder from the same buildx cache.
    assert_eq!(
        image.matches("docker/build-push-action").count(),
        2,
        "the image job builds the builder and the image, once each"
    );
}

// ---- agentd-build itself -------------------------------------------------

/// A scratch directory, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(tag: &str) -> Self {
        let p = std::env::temp_dir().join(format!(
            "agentd-build-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Scratch(p)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn executable(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    write(path, text);
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
}

/// What a fake `cargo` was asked: one line per invocation, its arguments.
/// `cargo build` writes a binary at `$CARGO_TARGET_DIR/release/agentd` that
/// answers `--build-info` with `$FAKE_FEATURES` and refuses a configuration
/// that names `payments`; `cargo deny` keeps the config it was given and
/// fails when `$FAKE_DENY` is `fail`. Every invocation also records the
/// AGENTD_* variables it saw.
const FAKE_CARGO: &str = r#"#!/bin/sh
log=$FAKE_LOG
printf 'cargo %s\n' "$*" >>"$log"
printf 'env AGENTD_REVISION=%s AGENTD_SOURCE_MODIFIED=%s AGENTD_BUILDER=%s\n' \
  "${AGENTD_REVISION-<unset>}" "${AGENTD_SOURCE_MODIFIED-<unset>}" "${AGENTD_BUILDER-<unset>}" >>"$log"
case $1 in
build)
  [ "${FAKE_BUILD-}" != fail ] || exit 101
  mkdir -p "$CARGO_TARGET_DIR/release"
  cat >"$CARGO_TARGET_DIR/release/agentd" <<EOF
#!/bin/sh
case \$1 in
--build-info) printf '{"name":"agentd","version":"0.0.0","revision":null,"source_modified":false,"features":%s,"target":"x86_64-unknown-linux-musl","profile":"release","rustc":"rustc 1.96.0","builder":null}\n' '$FAKE_FEATURES' ;;
--validate-config)
  if grep -q payments "\$3"; then
    echo '{"event":"config.invalid","msg":"security.payments needs the x402 feature"}' >&2
    exit 2
  fi
  echo '{"event":"config.valid"}' >&2 ;;
esac
EOF
  chmod +x "$CARGO_TARGET_DIR/release/agentd" ;;
deny)
  for a in "$@"; do
    [ "${next-}" = 1 ] && cp "$a" "$FAKE_DENY_CONFIG" && next=
    [ "$a" = --config ] && next=1
  done
  [ "${FAKE_DENY-}" != fail ] || { echo 'error[rejected]: a licence deny.toml does not allow' >&2; exit 1; }
  ;;
esac
"#;

/// The builder stage's `RUN` that records what the builder was told, as a
/// script: the one that writes `.agentd-build/SOURCE_DIGEST`.
fn record_step() -> String {
    // Each RUN with its continuation lines.
    let mut runs: Vec<String> = Vec::new();
    let mut in_run = false;
    for l in stage("builder").lines() {
        if in_run {
            let run = runs.last_mut().unwrap();
            run.push_str(l);
            run.push('\n');
        } else if let Some(run) = l.strip_prefix("RUN ") {
            runs.push(format!("{run}\n"));
        } else {
            continue;
        }
        in_run = l.ends_with('\\');
    }
    runs.into_iter()
        .find(|r| r.contains("SOURCE_DIGEST"))
        .expect("the builder stage has a RUN that writes SOURCE_DIGEST")
}

/// A source tree with what `agentd-build` reads, and the builder's own
/// files: the features file and the record of what the builder was told,
/// made by the Dockerfile's own step: when `revision` is given, the tree's
/// revision and digest and the builder's name.
struct Builder {
    dir: Scratch,
}

impl Builder {
    fn new(tag: &str, revision: Option<&str>) -> Self {
        let dir = Scratch::new(tag);
        let src = dir.0.join("src");
        for f in [
            "crates/agentd-cli/Cargo.toml",
            ".github/workflows/ci.yml",
            ".github/workflows/release.yml",
            "deny.toml",
        ] {
            write(&src.join(f), &read(f));
        }
        write(
            &dir.0.join("etc/features"),
            &read("packaging/builder/features"),
        );
        executable(&dir.0.join("bin/cargo"), FAKE_CARGO);
        executable(
            &dir.0.join("bin/agentd-build"),
            &format!(
                "#!/bin/sh\nexec sh {} \"$@\"\n",
                root().join("packaging/builder/agentd-build").display()
            ),
        );
        let b = Builder { dir };
        let made = match revision {
            Some(rev) => b.record(rev, "v9.9.9"),
            None => b.record("", ""),
        };
        assert!(made.status.success(), "{}", made.stderr);
        b
    }

    /// The builder stage's record, made as the Dockerfile makes it, given
    /// the build arguments REVISION and VERSION (empty: not passed).
    fn record(&self, revision: &str, version: &str) -> Run {
        // The builder's /etc/agentd-build is this one's etc, by variable:
        // the scratch path has characters the shell would read as syntax.
        let script = record_step().replace("/etc/agentd-build/", "\"$AGENTD_BUILD_ETC\"/");
        let out = Command::new("sh")
            .arg("-c")
            .arg(script)
            .current_dir(self.path("src"))
            .env("PATH", self.search_path())
            .env("AGENTD_BUILD_ETC", self.path("etc"))
            .env("AGENTD_BUILD_SOURCE", self.path("src"))
            .env("REVISION", revision)
            .env("VERSION", version)
            .output()
            .unwrap();
        Run::from(out)
    }

    /// PATH with the fake cargo and agentd-build first.
    fn search_path(&self) -> String {
        format!(
            "{}:{}",
            self.path("bin").display(),
            std::env::var("PATH").unwrap_or_default()
        )
    }

    fn path(&self, rel: &str) -> PathBuf {
        self.dir.0.join(rel)
    }

    fn log(&self) -> String {
        std::fs::read_to_string(self.path("cargo.log")).unwrap_or_default()
    }

    fn run(&self, args: &[&str], env: &[(&str, &str)]) -> Run {
        let out = Command::new("sh")
            .arg(root().join("packaging/builder/agentd-build"))
            .args(args)
            .current_dir(&self.dir.0)
            .env("PATH", self.search_path())
            .env("AGENTD_BUILD_ETC", self.path("etc"))
            .env("AGENTD_BUILD_SOURCE", self.path("src"))
            .env("CARGO_TARGET_DIR", self.path("target"))
            .env("FAKE_LOG", self.path("cargo.log"))
            .env("FAKE_DENY_CONFIG", self.path("deny-config.toml"))
            .env("AGENTD_REVISION", "from-the-environment")
            .envs(env.iter().copied())
            .output()
            .unwrap();
        Run::from(out)
    }
}

struct Run {
    status: std::process::ExitStatus,
    stdout: String,
    stderr: String,
}

impl From<std::process::Output> for Run {
    fn from(out: std::process::Output) -> Self {
        Run {
            status: out.status,
            stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
        }
    }
}

/// jq reads the binary's answer in agentd-build. A runner without it cannot
/// drive the steps after the build; CI has it, and there a missing jq fails.
fn jq_or_skip() -> bool {
    let have = Command::new("jq")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success());
    assert!(
        have || std::env::var_os("CI").is_none(),
        "jq is missing on a CI runner"
    );
    if !have {
        eprintln!("jq is not installed: the steps after the build are not driven here");
    }
    have
}

#[test]
fn it_refuses_before_it_compiles() {
    let b = Builder::new("refuse", None);
    for (args, says) in [
        (
            &["--features", "exec"][..],
            "refused: exec needs --allow exec: runs local commands",
        ),
        (&["--features", "a2a,exec"], "exec needs --allow exec"),
        (
            &["--features", "internal-mocks"],
            "refused: internal-mocks: test scaffolding",
        ),
        (
            &["--features", "a2a,not-a-feature"],
            "refused: agentd has no feature called not-a-feature",
        ),
        (&["--features", "default"], "default is not a feature"),
        (
            &["--features", "a2a", "--allow", "a2a"],
            "--allow a2a: a2a is not a feature this builder asks to confirm",
        ),
        (
            &["--features", "a2a", "--expect-features", "a2a"],
            "this build's features are a2a,tls, not a2a",
        ),
        (&["--features"], "needs a value (agentd-build --help)"),
        (&[], "say which features to build"),
        (&["--frobnicate"], "unknown argument: --frobnicate"),
    ] {
        let run = b.run(args, &[]);
        assert_eq!(run.status.code(), Some(2), "{args:?}: {}", run.stderr);
        assert!(run.stderr.contains(says), "{args:?} said:\n{}", run.stderr);
    }
    assert_eq!(b.log(), "", "a refused build compiled something");
}

/// A builder derived from this one ships a narrower features file; a feature
/// it leaves out cannot come back in through one that implies it, or as a
/// default.
#[test]
fn a_narrower_features_file_offers_less() {
    let b = Builder::new("narrow", None);
    let narrow: String = read("packaging/builder/features")
        .lines()
        .filter(|l| !l.starts_with("aauth\t") && !l.starts_with("tls\t"))
        .map(|l| format!("{l}\n"))
        .collect();
    write(&b.path("etc/features"), &narrow);
    let run = b.run(&["--features", "sign"], &[]);
    assert_eq!(run.status.code(), Some(2), "{}", run.stderr);
    assert!(
        run.stderr
            .contains("refused: aauth (sign turns it on) is not offered by this builder"),
        "{}",
        run.stderr
    );
    assert!(
        run.stderr
            .contains("refused: tls (sign turns it on) is not offered"),
        "{}",
        run.stderr
    );
    let run = b.run(&["--features", "cron"], &[]);
    assert!(
        run.stderr
            .contains("tls (on by default; --no-default-features leaves it out)"),
        "{}",
        run.stderr
    );
    let list = b.run(&["--list"], &[]);
    assert!(list.status.success() && !list.stdout.contains("\n  aauth "));
    assert_eq!(b.log(), "");
}

#[test]
fn list_shows_the_offer_and_the_tested_sets() {
    let b = Builder::new("list", None);
    let run = b.run(&["--list"], &[]);
    assert!(run.status.success(), "{}", run.stderr);
    for (name, tier, _) in features_file() {
        assert!(
            run.stdout.lines().any(|l| l
                .split_whitespace()
                .take(2)
                .eq([name.as_str(), tier.as_str()])),
            "--list does not show {name} as {tier}:\n{}",
            run.stdout
        );
    }
    let mut release = release_features();
    release.push("default".into());
    assert!(run.stdout.contains(&closure(&release).join(",")));
    // Every CI row is a tested set, as its closure.
    for row in ci_rows() {
        assert!(
            run.stdout.contains(&format!("ci.yml's row \"{row}\"")),
            "--list leaves out the CI row {row:?}"
        );
    }
}

#[test]
fn it_builds_exactly_what_was_asked_offline() {
    let b = Builder::new("cargo", None);
    let run = b.run(
        &["--features", "cron,a2a,metrics"],
        &[("FAKE_BUILD", "fail")],
    );
    assert_eq!(run.status.code(), Some(1), "{}", run.stderr);
    assert!(run.stderr.contains("the build failed"), "{}", run.stderr);
    let log = b.log();
    assert!(
        log.lines()
            .any(|l| l == "cargo build --release --locked --offline -p agentd-cli --features a2a,cron,metrics"),
        "{log}"
    );
    // Outside every set CI tests: said, with the nearest one.
    assert!(
        run.stderr.contains(
            "warning: CI does not test this combination (a2a,cron,metrics,tls); the nearest tested set is ci.yml's row \"--features a2a\": it adds cron,metrics"
        ),
        "{}",
        run.stderr
    );
    // A value the builder did not say never reaches the binary.
    assert!(
        log.contains(
            "env AGENTD_REVISION=<unset> AGENTD_SOURCE_MODIFIED=<unset> AGENTD_BUILDER=<unset>"
        ),
        "{log}"
    );

    let b = Builder::new("tested", None);
    let release = release_features().join(",");
    let run = b.run(&["--features", &release], &[("FAKE_BUILD", "fail")]);
    assert!(
        !run.stderr.contains("warning"),
        "the release set is tested:\n{}",
        run.stderr
    );
    let run = b.run(
        &["--features", "", "--no-default-features"],
        &[("FAKE_BUILD", "fail")],
    );
    assert!(
        run.stderr
            .contains("the nearest tested set is the default set: it lacks tls"),
        "{}",
        run.stderr
    );
    assert!(b.log().lines().any(
        |l| l == "cargo build --release --locked --offline -p agentd-cli --no-default-features"
    ));
}

#[test]
fn it_says_whether_the_source_is_the_builders() {
    let b = Builder::new("modified", Some("0123abc"));
    b.run(&["--features", "cron"], &[("FAKE_BUILD", "fail")]);
    assert!(
        b.log().contains(
            "env AGENTD_REVISION=0123abc AGENTD_SOURCE_MODIFIED=false AGENTD_BUILDER=agentd-builder 9.9.9"
        ),
        "{}",
        b.log()
    );
    write(&b.path("src/crates/agentd-cli/src/patched.rs"), "// mine\n");
    let run = b.run(&["--features", "cron"], &[("FAKE_BUILD", "fail")]);
    assert!(
        run.stderr.contains("source_modified: true"),
        "{}",
        run.stderr
    );
    assert!(
        b.log().contains("AGENTD_SOURCE_MODIFIED=true"),
        "{}",
        b.log()
    );
}

/// What a build reports comes from what the builder was told, and a builder
/// told nothing — built from a checkout, or by a plain `docker build .` —
/// records nothing: a build in it reports `revision`, `source_modified` and
/// `builder` as `null`, not a placeholder that reads as an answer.
#[test]
fn the_builder_records_only_what_it_was_told() {
    let builder = stage("builder");
    for arg in [r#"ARG VERSION="""#, r#"ARG REVISION="""#] {
        assert!(
            builder.lines().any(|l| l == arg),
            "the builder stage declares {arg}, with no placeholder default"
        );
    }

    let b = Builder::new("told-nothing", None);
    for f in [
        "src/.agentd-build/REVISION",
        "src/.agentd-build/SOURCE_DIGEST",
        "etc/builder",
    ] {
        assert!(!b.path(f).exists(), "a builder told nothing wrote {f}");
    }

    let b = Builder::new("told", Some("0123abc"));
    let digest = b.run(&["--source-digest"], &[]);
    assert!(digest.status.success(), "{}", digest.stderr);
    assert!(digest.stdout.starts_with("sha256:"), "{}", digest.stdout);
    let file = |f: &str| std::fs::read_to_string(b.path(f)).unwrap();
    assert_eq!(file("src/.agentd-build/REVISION"), "0123abc\n");
    assert_eq!(file("src/.agentd-build/SOURCE_DIGEST"), digest.stdout);
    assert_eq!(
        file("etc/builder"),
        "agentd-builder 9.9.9\n",
        "the builder is named after its release, without the tag's v"
    );
}

#[test]
fn it_judges_the_builds_own_graph_and_writes_what_it_built() {
    if !jq_or_skip() {
        return;
    }
    let b = Builder::new("full", Some("0123abc"));
    let features = r#"["a2a","cron","metrics","tls"]"#;
    let out = b.path("out");
    let out_arg = out.display().to_string();

    // cargo deny is asked about this build's graph, and only it.
    let run = b.run(
        &["--features", "a2a,cron,metrics", "--out", &out_arg],
        &[("FAKE_FEATURES", features), ("FAKE_DENY", "fail")],
    );
    assert_eq!(run.status.code(), Some(1), "{}", run.stderr);
    assert!(
        run.stderr
            .contains("cargo deny refused this build's dependency graph")
    );
    assert!(!out.exists(), "a refused graph wrote an output");
    let deny_line = b
        .log()
        .lines()
        .find(|l| l.starts_with("cargo deny "))
        .unwrap()
        .to_string();
    assert!(
        deny_line.starts_with(
            "cargo deny --offline --locked --manifest-path crates/agentd-cli/Cargo.toml check --config "
        ) && deny_line.contains(" --exclude-dev ")
            && !deny_line.contains("--features"),
        "{deny_line}"
    );
    let config = std::fs::read_to_string(b.path("deny-config.toml")).unwrap();
    let graphs = config.lines().filter(|l| l.trim() == "[graph]").count();
    assert_eq!(graphs, 1, "one [graph], this build's:\n{config}");
    assert!(
        config.contains(
            "[graph]\nfeatures = [\"a2a\", \"cron\", \"metrics\"]\nno-default-features = false\n"
        ),
        "{config}"
    );
    assert!(config.contains("[licenses]") && config.contains("[advisories]"));

    // The binary must have what was asked for.
    let run = b.run(
        &["--features", "a2a,cron,metrics", "--out", &out_arg],
        &[("FAKE_FEATURES", r#"["a2a","tls"]"#)],
    );
    assert_eq!(run.status.code(), Some(1));
    assert!(
        run.stderr.contains(
            "the binary reports the features a2a,tls, and the build asked for a2a,cron,metrics,tls"
        ),
        "{}",
        run.stderr
    );

    // A configuration that needs what the build lacks fails the build.
    let config = b.path("pay.yaml");
    write(&config, "security:\n  payments: {}\n");
    let run = b.run(
        &[
            "--features",
            "a2a,cron,metrics",
            "--check-config",
            &config.display().to_string(),
            "--out",
            &out_arg,
        ],
        &[("FAKE_FEATURES", features)],
    );
    assert_eq!(run.status.code(), Some(1));
    assert!(
        run.stderr
            .contains("security.payments needs the x402 feature")
    );
    assert!(
        run.stderr
            .contains("pay.yaml does not validate against this build")
    );
    assert!(!out.exists());

    // And a build that passes writes the binary, its build info, and the
    // label line for the image that carries it.
    write(&b.path("fine.yaml"), "agent: {}\n");
    let run = b.run(
        &[
            "--features",
            "a2a,cron,metrics",
            "--check-config",
            "fine.yaml",
            "--out",
            &out_arg,
        ],
        &[("FAKE_FEATURES", features)],
    );
    assert!(run.status.success(), "{}", run.stderr);
    assert!(out.join("agentd").is_file());
    let info: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out.join("build-info.json")).unwrap())
            .unwrap();
    assert_eq!(
        info["features"],
        serde_json::json!(["a2a", "cron", "metrics", "tls"])
    );
    assert!(
        run.stdout
            .lines()
            .any(|l| l == r#"  LABEL dev.agentd.features="a2a,cron,metrics,tls""#),
        "{}",
        run.stdout
    );
}

/// The ask itself, on the binary this test build made.
#[test]
fn build_info_answers_anywhere() {
    let dir = Scratch::new("ask");
    // A config in the directory that does not parse: `--build-info`, like
    // `--version`, does not look for one.
    write(&dir.0.join("agentd.yaml"), "{{{ not yaml");
    let out = Command::new(env!("CARGO_BIN_EXE_agentd"))
        .arg("--build-info")
        .current_dir(&dir.0)
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let info: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(info["name"], "agentd");
    let features: Vec<String> = info["features"]
        .as_array()
        .unwrap()
        .iter()
        .map(|f| f.as_str().unwrap().to_string())
        .collect();
    let declared: Vec<String> = feature_graph().into_keys().collect();
    assert!(
        features.iter().all(|f| declared.contains(f)),
        "{features:?}"
    );
    assert_eq!(features.contains(&"tls".to_string()), cfg!(feature = "tls"));
    assert_eq!(features.contains(&"a2a".to_string()), cfg!(feature = "a2a"));
    assert_eq!(
        info["profile"],
        if cfg!(debug_assertions) {
            "debug"
        } else {
            "release"
        }
    );
}
