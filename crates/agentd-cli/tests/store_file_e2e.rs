// SPDX-License-Identifier: AGPL-3.0-only
//! The local-filesystem store against the real adapter and a real directory —
//! no mocks, no temp-file shims: every assertion here is about bytes that
//! actually landed on disk.
//!
//! The adapter is constructed directly (`FileStore::open`) rather than through
//! `store::open`, because what is under test is the ADAPTER's contract — the
//! `seq` CAS, atomic publication, the traversal guard and the instance lock —
//! not the config plumbing that selects it. Four properties, in the order they
//! matter:
//!
//! 1. **Round trip** — put/get/list/delete through the `Store` trait, a `seq`
//!    that does not advance coming back as `Conflict` with the stored seq, and
//!    a tombstone reading as absent while staying visible to `list`.
//! 2. **Atomicity** — a reader racing a writer never observes a partial file,
//!    and the residue a crash between `tmp` and `rename` leaves behind neither
//!    damages the previous envelope nor shows up as a key.
//! 3. **Traversal** — the security case. Ids reach this adapter from run ids,
//!    task ids and A2A context ids; `../..`, an absolute path, a raw `/` and a
//!    NUL must not put a byte outside the root.
//! 4. **Lock** — a second `open` on the same root fails, naming the pid that
//!    holds it, and the lock is released when the store is dropped.

use agentd::store::file::FileStore;
use agentd::store::{Envelope, PutOutcome, Store, StoreError};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// An envelope the way the runtime writes one: v2, kind/id/instance filled in,
/// `seq` matching the CAS argument (the adapter reads its CAS floor back out of
/// the stored envelope, so the two must agree).
fn env(kind: &str, id: &str, seq: u64, state: Value) -> Value {
    Envelope::new(kind, id, seq, "inst", None, state).to_value()
}

fn key(kind: &str, id: &str) -> String {
    agentd::store::key("agentd", "inst", kind, id)
}

/// Every path under `root`, relative and slash-joined, sorted — the whole tree
/// including the lock file and any temp residue, because a traversal test that
/// only looks at the files it expects cannot see the file it did not.
fn tree(root: &Path) -> BTreeSet<String> {
    fn walk(dir: &Path, root: &Path, out: &mut BTreeSet<String>) {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                walk(&p, root, out);
            } else {
                out.insert(
                    p.strip_prefix(root)
                        .unwrap_or(&p)
                        .to_string_lossy()
                        .replace('\\', "/"),
                );
            }
        }
    }
    let mut out = BTreeSet::new();
    walk(root, root, &mut out);
    out
}

/// An INDEPENDENT re-implementation of the adapter's segment encoding, so the
/// traversal test compares the on-disk tree against an oracle rather than
/// against the code that produced it. Unreserved set: `A-Za-z0-9-_`.
fn enc(seg: &str) -> String {
    seg.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b == b'-' || b == b'_' {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// The file a key is expected to land in, derived from the oracle encoder.
fn expected_file(k: &str) -> String {
    let segs: Vec<&str> = k.split('/').filter(|s| !s.is_empty()).collect();
    let mut parts: Vec<String> = segs.iter().map(|s| enc(s)).collect();
    let last = parts.len() - 1;
    parts[last] = format!("{}.json", parts[last]);
    parts.join("/")
}

fn tmp() -> tempfile::TempDir {
    tempfile::tempdir().expect("tempdir")
}

// ---------------------------------------------------------------------------
// 1. Round trip
// ---------------------------------------------------------------------------

#[test]
fn round_trip_put_get_list_delete_cas_and_tombstone() {
    let td = tmp();
    let root = td.path().join("state");
    let s = FileStore::open(&root).expect("open");
    assert_eq!(
        s.kind(),
        "file",
        "the adapter identifies itself for metrics"
    );

    let k = key("run", "01M06");
    assert_eq!(
        s.get(&k, None).unwrap(),
        None,
        "an absent key reads as None"
    );

    // put → get, byte for byte the envelope we handed it.
    assert_eq!(
        s.put(&k, 1, &env("run", "01M06", 1, json!({"step": "a"})))
            .unwrap(),
        PutOutcome::Ok
    );
    let got = s.get(&k, None).unwrap().expect("stored");
    let e = Envelope::from_value(got).expect("parses as a v2 envelope");
    assert_eq!(
        (e.seq, e.kind.as_str(), e.instance.as_str()),
        (1, "run", "inst")
    );
    assert_eq!(e.state, json!({"step": "a"}));

    // The CAS is the split-brain fence: a seq that does not EXCEED the stored
    // one is refused, and the refusal carries the stored seq so the caller can
    // tell "I am behind" from "the store is broken".
    for stale in [0u64, 1] {
        assert_eq!(
            s.put(
                &k,
                stale,
                &env("run", "01M06", stale, json!({"step": "clobber"}))
            )
            .unwrap(),
            PutOutcome::Conflict {
                latest_seq: Some(1)
            },
            "seq {stale} must not overwrite the stored seq 1"
        );
    }
    // …and the refused write left the stored envelope untouched.
    let e = Envelope::from_value(s.get(&k, None).unwrap().unwrap()).unwrap();
    assert_eq!(e.state, json!({"step": "a"}), "a conflict writes nothing");

    assert_eq!(
        s.put(&k, 2, &env("run", "01M06", 2, json!({"step": "b"})))
            .unwrap(),
        PutOutcome::Ok
    );
    // `get(seq)` is latest-only for this adapter (like `http`): a pinned seq
    // reads the latest, it does not resurrect history.
    for pin in [None, Some(1), Some(2)] {
        let e = Envelope::from_value(s.get(&k, pin).unwrap().unwrap()).unwrap();
        assert_eq!((e.seq, &e.state), (2, &json!({"step": "b"})), "pin {pin:?}");
    }

    // list over the instance prefix: keys come back in the ORIGINAL spelling,
    // decoded out of the directory names, with each record's seq.
    let k2 = key("memory", "notes");
    s.put(&k2, 7, &env("memory", "notes", 7, json!(["one"])))
        .unwrap();
    let listed = s.list("agentd/inst").unwrap();
    let pairs: Vec<(String, Option<u64>)> = listed.iter().map(|e| (e.key.clone(), e.seq)).collect();
    assert_eq!(
        pairs,
        vec![(k2.clone(), Some(7)), (k.clone(), Some(2))],
        "list returns decoded keys, sorted, with seqs"
    );
    // A narrower prefix is a directory walk, not a string filter.
    assert_eq!(
        s.list("agentd/inst/run").unwrap().len(),
        1,
        "the prefix selects one kind"
    );
    assert!(
        s.list("agentd/nobody").unwrap().is_empty(),
        "an unknown prefix is empty, not an error"
    );

    // A tombstone (state: null) is a WRITE, not a delete: it advances the seq,
    // reads back as absent, and stays visible to `list` so a restore can tell
    // "deleted at seq 3" from "never existed".
    assert_eq!(
        s.put(&k, 3, &env("run", "01M06", 3, Value::Null)).unwrap(),
        PutOutcome::Ok
    );
    assert_eq!(
        s.get(&k, None).unwrap(),
        None,
        "a tombstone reads as absent"
    );
    assert_eq!(
        s.list("agentd/inst/run").unwrap()[0].seq,
        Some(3),
        "the tombstone still fences later writers at seq 3"
    );
    assert_eq!(
        s.put(&k, 3, &env("run", "01M06", 3, json!({"step": "c"})))
            .unwrap(),
        PutOutcome::Conflict {
            latest_seq: Some(3)
        },
        "a tombstone is a CAS floor like any other record"
    );

    // delete unlinks; it is idempotent, because a caller retrying a delete
    // after a partial failure must not get an error for winning twice.
    s.delete(&k).unwrap();
    assert_eq!(s.get(&k, None).unwrap(), None);
    assert!(
        s.list("agentd/inst/run").unwrap().is_empty(),
        "the key is gone from list"
    );
    s.delete(&k).unwrap();
    // The unrelated key is untouched by the delete.
    assert!(s.get(&k2, None).unwrap().is_some());

    // On disk: 0700 dirs, 0600 files, nothing but the lock and the one key.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let f = root.join(expected_file(&k2));
        assert_eq!(
            fs::metadata(&f).unwrap().permissions().mode() & 0o777,
            0o600,
            "state files are 0600: they hold conversation content"
        );
        assert_eq!(
            fs::metadata(f.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700,
            "state directories are 0700"
        );
    }
    assert_eq!(
        tree(&root),
        BTreeSet::from([".lock".to_string(), expected_file(&k2)]),
        "one file per live key, plus the instance lock"
    );
}

// ---------------------------------------------------------------------------
// 2. Atomicity
// ---------------------------------------------------------------------------

/// A reader hammering the target path while a writer publishes envelopes whose
/// size swings by four orders of magnitude (10 bytes ↔ 128 KiB) must never
/// observe a partial file.
///
/// What this proves: of the many states sampled at the key's path while it was
/// being rewritten 240 times, every one was a complete, self-consistent
/// envelope — the payload's real length always equalled the length the envelope
/// declares. An adapter that wrote in place would fail this within a handful of
/// iterations, because a reader lands between the truncate and the last byte.
///
/// What it does NOT prove: it is a SAMPLING test, so it can only ever falsify —
/// a passing run is evidence, not a proof of atomicity. (Hence the read counter:
/// a run where the reader never got a look would otherwise "pass" having tested
/// nothing.) Nor does it say anything about durability across a power cut —
/// that is the `fsync` pair, which userspace cannot observe. It is about
/// PUBLICATION: no partial state ever becomes visible at the key's path.
///
/// Scope: ONE writer, which is the runtime's shape — the reactor is a blocking
/// single-writer loop. Two THREADS writing the same key concurrently are out of
/// scope and are not safe: the adapter names its temp file per PROCESS
/// (`.<file>.tmp.<pid>`), so two writers in one process share it. The single-
/// writer invariant is what makes that fine; a caller that ever breaks it needs
/// the temp name made unique first.
#[test]
fn a_reader_racing_a_writer_never_sees_a_partial_file() {
    let td = tmp();
    let root = td.path().join("state");
    let s = Arc::new(FileStore::open(&root).expect("open"));
    let k = key("run", "hot");
    let path = root.join(expected_file(&k));

    let stop = Arc::new(AtomicBool::new(false));
    let reads = Arc::new(AtomicU64::new(0));
    let small = Arc::new(AtomicU64::new(0));
    let large = Arc::new(AtomicU64::new(0));

    let reader = {
        let (stop, reads, small, large, path) = (
            stop.clone(),
            reads.clone(),
            small.clone(),
            large.clone(),
            path.clone(),
        );
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                let Ok(bytes) = fs::read(&path) else { continue }; // not yet published
                let v: Value = serde_json::from_slice(&bytes).unwrap_or_else(|e| {
                    panic!(
                        "torn read: {} bytes at {} did not parse ({e}) — the write was not atomic",
                        bytes.len(),
                        path.display()
                    )
                });
                let declared = v["state"]["len"].as_u64().expect("state.len") as usize;
                let actual = v["state"]["filler"].as_str().expect("state.filler").len();
                assert_eq!(
                    declared, actual,
                    "a published envelope is internally inconsistent: declared {declared}, \
                     got {actual} — a partial file became visible"
                );
                reads.fetch_add(1, Ordering::Relaxed);
                if actual > 100_000 { &large } else { &small }.fetch_add(1, Ordering::Relaxed);
            }
        })
    };

    // Alternate a ~10-byte payload with a ~256 KiB one: consecutive writes
    // differ by ~26,000x, so any leftover tail of the previous envelope, or any
    // prefix of the next, is caught by the length check above.
    for seq in 1..=240u64 {
        let n = if seq % 2 == 0 { 10 } else { 128 * 1024 };
        let filler = "x".repeat(n);
        let st = json!({"len": n, "filler": filler});
        assert_eq!(
            s.put(&k, seq, &env("run", "hot", seq, st)).unwrap(),
            PutOutcome::Ok
        );
    }
    stop.store(true, Ordering::Relaxed);
    reader.join().expect("the reader saw only whole envelopes");

    // A race that never actually raced proves nothing, so assert we observed
    // both sizes: the reader really did read across many publications.
    let (r, sm, lg) = (
        reads.load(Ordering::Relaxed),
        small.load(Ordering::Relaxed),
        large.load(Ordering::Relaxed),
    );
    assert!(
        r > 50,
        "only {r} reads landed — the race did not happen, the test proved nothing"
    );
    assert!(
        sm > 0 && lg > 0,
        "saw {sm} small / {lg} large envelopes — sizes did not interleave"
    );

    // Publication is by rename, so nothing is left behind: no temp file
    // survives a completed write.
    let leftovers: Vec<String> = tree(&root)
        .into_iter()
        .filter(|p| p.contains(".tmp."))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files survived completed writes: {leftovers:?}"
    );
}

/// The crash case, reconstructed rather than simulated by killing a process:
/// put the filesystem into exactly the state a SIGKILL between `create(tmp)`
/// and `rename` leaves — a complete previous envelope plus a truncated temp
/// file beside it — and assert the store reads through it unharmed.
///
/// Honest scope: this asserts the RECOVERY property — the previous envelope
/// survives, and the residue is invisible and is not mistaken for a key. It
/// does not prove the kernel never publishes a half-renamed file; `rename(2)`
/// within one directory is what guarantees that, and the test above is the
/// evidence that the adapter uses it.
#[test]
fn a_crash_between_tmp_and_rename_leaves_the_previous_envelope_intact() {
    // Reopens the root after a drop, so it needs the same fork-free window the
    // lock tests do — see `FORK_FREE`.
    let _no_forks = FORK_FREE.lock().unwrap_or_else(|e| e.into_inner());
    let td = tmp();
    let root = td.path().join("state");
    let s = FileStore::open(&root).expect("open");
    let k = key("run", "crashy");
    s.put(
        &k,
        4,
        &env("run", "crashy", 4, json!({"step": "committed"})),
    )
    .unwrap();

    // The residue: same directory, the adapter's own naming (`.<file>.tmp.<pid>`),
    // holding the first half of a bigger envelope that never got renamed.
    let file = root.join(expected_file(&k));
    let dir = file.parent().unwrap();
    let residue = dir.join(format!(
        ".{}.tmp.{}",
        file.file_name().unwrap().to_string_lossy(),
        424242
    ));
    let partial =
        serde_json::to_vec(&env("run", "crashy", 5, json!({"step": "interrupted"}))).unwrap();
    fs::write(&residue, &partial[..partial.len() / 2]).unwrap();

    // Restart: a fresh adapter over the same root, as a restarting process does.
    drop(s);
    let s = FileStore::open(&root).expect("reopen after the crash");
    let e =
        Envelope::from_value(s.get(&k, None).unwrap().expect("the committed envelope")).unwrap();
    assert_eq!(
        (e.seq, &e.state),
        (4, &json!({"step": "committed"})),
        "seq 4 survived intact"
    );
    assert_eq!(
        s.list("agentd/inst").unwrap().len(),
        1,
        "the truncated temp file is not a key — list must not surface it"
    );
    assert_eq!(
        s.put(&k, 5, &env("run", "crashy", 5, json!({"step": "redone"})))
            .unwrap(),
        PutOutcome::Ok,
        "the interrupted write can simply be redone: the CAS floor is still 4"
    );
    let e = Envelope::from_value(s.get(&k, None).unwrap().unwrap()).unwrap();
    assert_eq!(e.state, json!({"step": "redone"}));
}

// ---------------------------------------------------------------------------
// 3. Traversal — the security case
// ---------------------------------------------------------------------------

/// Ids reach this adapter from run ids, task ids and A2A context ids — values
/// that crossed the network. A hostile id must not put a byte outside the root,
/// and it must not become the *same* file as some other id either.
///
/// The root is buried several levels inside the temp dir and the sentinels sit
/// at every level above it, so an escape of any depth lands where this test can
/// see it instead of somewhere in the real filesystem.
#[test]
fn hostile_ids_never_write_outside_the_root() {
    let td = tmp();
    let base = td.path().to_path_buf();
    let root = base.join("a/b/c/state");
    let s = FileStore::open(&root).expect("open");

    let before: BTreeSet<String> = tree(&base);
    assert_eq!(before, BTreeSet::from(["a/b/c/state/.lock".to_string()]));

    // Each of these is written under a key whose ID is hostile. All of them
    // must succeed (they are legal opaque ids) and all of them must land INSIDE
    // the root — refusing them is not required, containing them is.
    let contained = [
        "../../../../../../etc/passwd", // classic relative escape
        "..",                           // the bare parent
        ".",                            // the current directory
        "/etc/passwd",                  // absolute
        "/",                            // absolute root
        "a/b",                          // a raw separator: nesting, not escape
        "..%2f..%2fpwned",              // pre-encoded, to catch a decode-then-use bug
        "....//....//pwned",            // the doubled form that defeats naive stripping
        "sub/../../../pwned",           // mixed
        "-",                            // an unreserved-looking oddity
        "ünïcøde",                      // multi-byte: encoded per byte
        " leading and trailing ",       // spaces
        "CON",                          // reserved on Windows, ordinary here
    ];
    let mut expected: BTreeSet<String> = BTreeSet::from([".lock".to_string()]);
    for id in contained {
        let k = key("run", id);
        assert_eq!(
            s.put(&k, 1, &env("run", id, 1, json!({"id": id}))).unwrap(),
            PutOutcome::Ok,
            "id {id:?} is a legal opaque id"
        );
        expected.insert(expected_file(&k));
        // Round trip: containment must not cost identity — the id reads back.
        let e = Envelope::from_value(s.get(&k, None).unwrap().expect("readable")).unwrap();
        assert_eq!(e.state, json!({"id": id}), "id {id:?} round-trips");
    }

    // A NUL cannot appear in a path at all, so it is REFUSED rather than
    // encoded — the one input that is a mapping error, not a containment case.
    let k = key("run", "nul\0byte");
    match s.put(&k, 1, &env("run", "x", 1, json!(1))) {
        Err(StoreError::Mapping(m)) => assert!(m.contains("NUL"), "unhelpful message: {m}"),
        other => panic!("a NUL id must be refused, got {other:?}"),
    }
    match s.get(&k, None) {
        Err(StoreError::Mapping(_)) => {}
        other => panic!("a NUL id must be refused on read too, got {other:?}"),
    }
    // …and an empty key has nowhere to go.
    assert!(matches!(
        s.put("", 1, &json!({})),
        Err(StoreError::Mapping(_))
    ));
    assert!(matches!(
        s.put("///", 1, &json!({})),
        Err(StoreError::Mapping(_))
    ));

    // The whole tree, exactly. Compared against the independent oracle encoder
    // above, so this fails both if something escaped AND if the on-disk layout
    // silently changed shape.
    let after = tree(&root);
    assert_eq!(
        after, expected,
        "the root holds exactly the keys that were put"
    );

    // Nothing appeared anywhere above the root — not in its parent, not at any
    // level of the temp tree, not under the names the hostile ids were reaching
    // for.
    let outside: Vec<String> = tree(&base)
        .into_iter()
        .filter(|p| !p.starts_with("a/b/c/state/"))
        .collect();
    assert!(
        outside.is_empty(),
        "files appeared outside the root: {outside:?}"
    );
    for probe in [
        "a/b/c/passwd",
        "a/b/pwned",
        "a/pwned",
        "pwned",
        "passwd",
        "etc",
    ] {
        assert!(!base.join(probe).exists(), "an escape landed at {probe}");
    }
    // Belt and braces: every file's canonical path is under the canonical root,
    // which catches an escape through a symlink or a `..` the tree walk above
    // would have followed back into range.
    let croot = root.canonicalize().unwrap();
    for rel in &after {
        let p = root.join(rel).canonicalize().unwrap();
        assert!(
            p.starts_with(&croot),
            "{} is not under {}",
            p.display(),
            croot.display()
        );
        assert!(
            !p.components()
                .any(|c| c.as_os_str() == ".." || c.as_os_str() == "."),
            "{} still carries a relative component",
            p.display()
        );
    }

    // Distinct ids stay distinct: containment by collapsing everything onto one
    // file would "pass" every assertion above and silently merge two agents'
    // runs. One file per id, and each holds its own id.
    assert_eq!(
        after.len(),
        contained.len() + 1,
        "one file per hostile id (plus .lock): ids collided on disk"
    );
    // Listed from the INSTANCE prefix, not `…/run`: the id `/` collapses to
    // no id segment at all and lands beside the `run` directory, which is
    // exactly the kind of edge a narrower walk would quietly miss.
    let listed = s.list("agentd/inst").unwrap();
    let keys: BTreeSet<String> = listed.into_iter().map(|e| e.key).collect();
    for id in contained {
        // `list` decodes the segments back, so a key round-trips except for the
        // empty segments a `/`-bearing id contributes (`//` collapses).
        let want: String = key("run", id)
            .split('/')
            .filter(|s| !s.is_empty())
            .collect::<Vec<_>>()
            .join("/");
        assert!(
            keys.contains(&want),
            "list lost {id:?} (wanted {want:?}) — have {keys:?}"
        );
    }
}

// ---------------------------------------------------------------------------
// 4. The instance lock
// ---------------------------------------------------------------------------

/// Serialises the two tests that care when a lock is *released* against the one
/// that forks a child process. A `fork` hands the child a copy of every open
/// descriptor, and `flock` belongs to the open file DESCRIPTION, so between the
/// `fork` and the `exec` (where `O_CLOEXEC` finally closes it) the child holds
/// the parent's lock too — closing the parent's descriptor in that window does
/// not release it. Cargo runs these tests as threads of one process, so without
/// this guard the release-on-drop assertion below fails a few runs in a hundred.
/// (The same window exists for real: agentd re-execs itself to spawn a subagent.
/// It closes at `exec`, so it is measured in microseconds, but it is not zero.)
static FORK_FREE: Mutex<()> = Mutex::new(());

/// Two writers sharing a directory would interleave silently, so the adapter
/// refuses to be the second one. The message has to name the holder's pid: the
/// operator's next move (kill it, or give this instance its own root) depends
/// on which process it is.
#[test]
#[cfg(unix)]
fn a_second_store_on_the_same_root_is_refused_and_names_the_pid() {
    let _no_forks = FORK_FREE.lock().unwrap_or_else(|e| e.into_inner());
    let td = tmp();
    let root = td.path().join("state");
    let first = FileStore::open(&root).expect("the first open takes the lock");

    let err = match FileStore::open(&root) {
        Err(StoreError::Io(m)) => m,
        Err(other) => panic!("the refusal must be an Io error, got {other:?}"),
        Ok(_) => panic!("a second open on the same root must fail"),
    };
    let me = std::process::id();
    assert!(
        err.contains(&format!("pid {me}")),
        "the message must name the holder: {err}"
    );
    assert!(
        err.contains(&root.display().to_string()),
        "…and the directory being fought over: {err}"
    );
    assert!(
        err.contains("agent.name") || err.contains("store.file.path"),
        "…and what to do about it: {err}"
    );

    // The lock is per-root, not global: an unrelated instance is unaffected.
    let other_root = td.path().join("other");
    let second = FileStore::open(&other_root).expect("a different root is a different lock");
    assert_eq!(second.root(), other_root.as_path());

    // Dropping the store releases the lock, so a restart of the same instance
    // (or a test) reopens its own state rather than locking itself out.
    //
    // Bounded, not instant: a lock released by closing a descriptor is only
    // really gone once every COPY of that open file description is gone, and a
    // `fork` anywhere else in this process (see `FORK_FREE`) transiently makes
    // one. The assertion is still "it is released" — a lock that is never
    // released fails here after two seconds instead of immediately.
    drop(first);
    let deadline = Instant::now() + Duration::from_secs(2);
    let reopened = loop {
        match FileStore::open(&root) {
            Ok(s) => break s,
            Err(e) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
                let _ = e;
            }
            Err(e) => panic!("the lock was not released on drop: {e}"),
        }
    };
    assert_eq!(reopened.root(), root.as_path());
    assert!(
        FileStore::open(&root).is_err(),
        "…and the reopened store now holds it"
    );
}

/// The lock is held by a *process*, and the message names that process — so a
/// live daemon in another process is what an operator actually hits. Prove it
/// with a real second process rather than a second file descriptor.
#[test]
#[cfg(unix)]
fn the_lock_is_held_across_processes() {
    let _no_forks = FORK_FREE.lock().unwrap_or_else(|e| e.into_inner());
    let td = tmp();
    let root = td.path().join("state");
    let holder = FileStore::open(&root).expect("open");
    // `flock` is inherited by a fork but the lock belongs to the open file
    // description, so a child that opens the file itself is a genuine second
    // claimant. Use a shell to avoid depending on a test binary path.
    let out = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!(
            "flock -n {} -c true; echo $?",
            shell_quote(&root.join(".lock"))
        ))
        .output();
    match out {
        Ok(o) if o.status.success() => {
            let code = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if code == "127" {
                eprintln!("skipped: flock(1) not installed");
            } else {
                assert_ne!(
                    code, "0",
                    "another process acquired the lock while we hold it"
                );
            }
        }
        // `flock(1)` is not installed everywhere; the in-process case above is
        // the assertion that must always run, this one is corroboration.
        _ => eprintln!("skipped: flock(1) unavailable"),
    }
    drop(holder);
}

fn shell_quote(p: &Path) -> String {
    format!("'{}'", p.display().to_string().replace('\'', r"'\''"))
}

/// The pid in the message is read out of the lock file, so a stale one written
/// by a process that died must not fool the next start into refusing.
#[test]
#[cfg(unix)]
fn a_stale_lock_file_from_a_dead_process_does_not_block_startup() {
    let td = tmp();
    let root = td.path().join("state");
    fs::create_dir_all(&root).unwrap();
    // What a SIGKILL leaves: the file with a pid in it, no flock held.
    fs::write(root.join(".lock"), "999999").unwrap();
    let s = FileStore::open(&root).expect("a stale lock FILE is not a held lock");
    let k = key("run", "after-restart");
    assert_eq!(
        s.put(&k, 1, &env("run", "after-restart", 1, json!(true)))
            .unwrap(),
        PutOutcome::Ok
    );
    let pid = fs::read_to_string(root.join(".lock")).unwrap();
    assert_eq!(
        pid.trim(),
        std::process::id().to_string(),
        "the holder rewrites the lock file with its own pid"
    );
}

/// A sanity net for the timing-sensitive tests above: nothing here should take
/// long enough to matter, and a hang in the lock path (a BLOCKING `flock`, say)
/// would otherwise show up only as a stuck CI job.
#[test]
#[cfg(unix)]
fn a_contended_open_fails_fast_rather_than_blocking() {
    let td = tmp();
    let root = td.path().join("state");
    let _held = FileStore::open(&root).expect("open");
    let t0 = Instant::now();
    assert!(FileStore::open(&root).is_err());
    assert!(
        t0.elapsed() < Duration::from_secs(2),
        "the contended open took {:?} — LOCK_NB is what makes this a startup error",
        t0.elapsed()
    );
}

/// The property the atomicity test leans on: `list` reports what is on disk
/// after a fresh `open`, so a restart really does find its state again.
/// (Identity is `agent.name`, so the SAME root plus the SAME instance segment
/// is all a restart needs.)
#[test]
fn a_restart_finds_its_state_under_the_same_instance_segment() {
    // Same reason as above: this test closes a store and reopens the root.
    let _no_forks = FORK_FREE.lock().unwrap_or_else(|e| e.into_inner());
    let td = tmp();
    let root = td.path().join("state");
    let mut want: Vec<(String, u64)> = Vec::new();
    {
        let s = FileStore::open(&root).expect("open");
        for (kind, id, seq) in [("run", "r1", 3), ("timer", "t1", 1), ("memory", "m1", 9)] {
            let k = key(kind, id);
            s.put(&k, seq, &env(kind, id, seq, json!({"k": kind})))
                .unwrap();
            want.push((k, seq));
        }
        // A different instance shares the root only if an operator points two
        // instances at it; the keys stay apart regardless.
        s.put(
            &agentd::store::key("agentd", "other", "run", "r1"),
            1,
            &env("run", "r1", 1, json!({"k": "other"})),
        )
        .unwrap();
    }
    let s = FileStore::open(&root).expect("reopen");
    want.sort();
    let got: Vec<(String, u64)> = s
        .list("agentd/inst")
        .unwrap()
        .into_iter()
        .map(|e| (e.key, e.seq.unwrap_or(0)))
        .collect();
    assert_eq!(got, want, "every key survived the restart, with its seq");
    assert_eq!(
        s.list("agentd").unwrap().len(),
        want.len() + 1,
        "the other instance's key is there, under its own segment"
    );
}

/// The adapter is `Send + Sync` and the runtime holds it behind an `Arc`, so
/// concurrent readers must be safe against a writer. (Concurrent WRITERS to one
/// key are the reactor's single-writer invariant, not this adapter's — see the
/// note in `store/file.rs`.)
#[test]
fn concurrent_readers_of_distinct_keys_are_consistent() {
    let td = tmp();
    let root = td.path().join("state");
    let s = Arc::new(FileStore::open(&root).expect("open"));
    for i in 0..20u64 {
        let k = key("run", &format!("r{i}"));
        s.put(
            &k,
            i + 1,
            &env("run", &format!("r{i}"), i + 1, json!({"i": i})),
        )
        .unwrap();
    }
    let handles: Vec<_> = (0..4)
        .map(|_| {
            let s = s.clone();
            std::thread::spawn(move || {
                for i in 0..20u64 {
                    let e = Envelope::from_value(
                        s.get(&key("run", &format!("r{i}")), None).unwrap().unwrap(),
                    )
                    .unwrap();
                    assert_eq!(e.state, json!({"i": i}));
                }
                s.list("agentd/inst").unwrap().len()
            })
        })
        .collect();
    for h in handles {
        assert_eq!(h.join().unwrap(), 20);
    }
}

/// A file that is not a valid envelope is a `Corrupt` error naming the file,
/// not a panic and not a silent `None`: an operator who edited state by hand,
/// or a half-restored backup, has to be told which file to look at.
#[test]
fn an_unparseable_record_is_reported_with_its_path() {
    let td = tmp();
    let root = td.path().join("state");
    let s = FileStore::open(&root).expect("open");
    let k = key("run", "bad");
    s.put(&k, 1, &env("run", "bad", 1, json!({"ok": true})))
        .unwrap();
    let path: PathBuf = root.join(expected_file(&k));
    fs::write(&path, b"{not json").unwrap();
    match s.get(&k, None) {
        Err(StoreError::Corrupt(m)) => assert!(
            m.contains(&path.display().to_string()),
            "the diagnostic must name the file: {m}"
        ),
        other => panic!("a corrupt record must surface as Corrupt, got {other:?}"),
    }
}
