// SPDX-License-Identifier: AGPL-3.0-only
//! The command ops: the reserved built-in set, and what one instance serves.

use crate::config::v2::Settings;

/// Is `op` one of the built-in command ops?
///
/// The built-in surface is RESERVED: a workflow's `a2a` start node may not
/// declare a command that collides with one, and the listener dispatches a
/// built-in to its own handler even if one somehow does. Both halves matter —
/// the ops carry their own authorization (`admin.*` is operator-only), and a
/// declared command takes the inbox path where that check does not run. A
/// workflow able to claim `admin.drain` could quietly shadow an operator's
/// drain control, and where the model may create workflows, that author is
/// the model.
pub fn is_builtin_op(op: &str) -> bool {
    // Every op any build can serve, not just this instance's enabled subset:
    // reserving a name only while a feature is on would make the collision
    // appear the day `interface.enabled` flipped.
    const ALL: &[&str] = &[
        "status",
        "config",
        "config.set",
        "workflow.run",
        "workflow.status",
        "workflow.cancel",
        "workflow.signal",
        "subagent.send",
        "subagent.kill",
        "subagent.status",
        "subagent.get",
        "plan.get",
        "ask_human",
        "admin.drain",
        "admin.lameduck",
        "admin.pause",
        "admin.resume",
        "admin.cancel",
        "interface.info",
        "conversation.get",
        "run.get",
        "debug.events",
        "pairing.code",
    ];
    ALL.contains(&op)
}

/// The command ops an instance serves, in ONE place: the agent card renders
/// them as skills, the command extension declares them, and `--capabilities`
/// reports them. Three views, one list — they cannot disagree (they did: the
/// manifest listed ops the card never mentioned).
pub fn command_ops_of(s: &Settings) -> Vec<&'static str> {
    let mut ops = vec![
        "status",
        "config",
        "workflow.run",
        "workflow.status",
        "workflow.cancel",
        "workflow.signal",
        "subagent.send",
        "subagent.kill",
        "subagent.status",
        "plan.get",
        "admin.drain",
        "admin.lameduck",
        "admin.pause",
        "admin.resume",
        "admin.cancel",
    ];
    ops.extend(interface_ops_of(s));
    ops
}

/// The ops the DISPLAY surface owns, in the order `interface.info` reports them.
///
/// Split out only because `interface.info` answers with this subset while the
/// card wants the whole set — one list, not two. `config.set`, `subagent.get`
/// and `pairing.code` were dispatched by the listener and advertised nowhere
/// until this existed, which is exactly the under-reporting the "one list feeds
/// three views" invariant is supposed to prevent.
pub fn interface_ops_of(s: &crate::config::v2::Settings) -> Vec<&'static str> {
    if !s.interface.enabled {
        return Vec::new();
    }
    let mut ops = vec!["interface.info", "config.set"];
    if s.interface.debug {
        ops.extend([
            "conversation.get",
            "run.get",
            "subagent.get",
            "debug.events",
        ]);
    }
    if s.interface.pairing.enabled {
        ops.push("pairing.code");
    }
    ops
}
