// SPDX-License-Identifier: Apache-2.0
//! Interactive entry for missing secrets (`--prompt-missing`).
//!
//! The shape mirrors `agentd login`: a deployment that is missing credentials
//! stops and asks the person in front of it, one value at a time, instead of
//! failing once per missing value across repeated restarts.
//!
//! Two hard rules keep this from becoming a footgun:
//!
//! - **Opt-in, and only with a controlling terminal.** The gate is opening
//!   `/dev/tty`, not guessing from stdin: a daemon under systemd or in a pod
//!   has no controlling terminal, so the open fails and startup fails loudly
//!   with the missing-reference list — never a silent hang on a prompt nobody
//!   can see.
//! - **Entered values live in process memory only** (`sec::secret`'s prompted
//!   store). They are never written to the config file (how secrets end up in
//!   git), never exported to the environment (children would inherit them),
//!   and a restart re-prompts — the honest cost of persisting nothing.

use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};

static REQUESTED: AtomicBool = AtomicBool::new(false);

/// Record that `--prompt-missing` was on the command line (consumed by the CLI
/// shell before the settings model sees argv, like `--fresh`).
pub fn request_prompt_missing() {
    REQUESTED.store(true, Ordering::Relaxed);
}

/// Whether the operator asked to be prompted for missing values.
pub fn prompt_missing_requested() -> bool {
    REQUESTED.load(Ordering::Relaxed)
}

/// Read one secret from the controlling terminal, echo off.
///
/// Errors when there is no controlling terminal — which is the correct answer
/// for a daemonized process, not a condition to work around.
pub fn read_secret_from_tty(label: &str) -> Result<String, String> {
    let mut tty = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/tty")
        .map_err(|e| format!("no controlling terminal ({e}) — --prompt-missing needs one"))?;
    write!(tty, "{label}: ").map_err(|e| e.to_string())?;
    tty.flush().ok();

    // Echo off for the read; restored whatever happens after it.
    let fd = std::os::fd::AsRawFd::as_raw_fd(&tty);
    let mut term: libc::termios = unsafe { std::mem::zeroed() };
    let had_termios = unsafe { libc::tcgetattr(fd, &mut term) } == 0;
    let saved = term;
    if had_termios {
        term.c_lflag &= !libc::ECHO;
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &term) };
    }
    let mut value = Vec::new();
    let mut byte = [0u8; 1];
    let read = loop {
        match tty.read(&mut byte) {
            Ok(0) => break Err("EOF before a value was entered".to_string()),
            Ok(_) if byte[0] == b'\n' => break Ok(()),
            Ok(_) => value.push(byte[0]),
            Err(e) => break Err(e.to_string()),
        }
    };
    if had_termios {
        unsafe { libc::tcsetattr(fd, libc::TCSANOW, &saved) };
    }
    let _ = writeln!(tty);
    read?;
    let s = String::from_utf8_lossy(&value);
    let s = s.trim_end_matches('\r').trim();
    if s.is_empty() {
        return Err("empty value".to_string());
    }
    Ok(s.to_string())
}
