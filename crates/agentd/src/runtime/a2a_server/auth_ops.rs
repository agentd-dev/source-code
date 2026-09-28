// SPDX-License-Identifier: AGPL-3.0-only
//! The `auth.*` command ops: the operator's side of the device grant and the
//! session list.

use crate::a2a::Principal;
use crate::runtime::reactor::Runtime;
use serde_json::Value;

/// Run an `auth.*` op. No row routes here until the device grant lands, so
/// every op this sees is one it does not implement.
pub(super) fn handle(_rt: &mut Runtime, _principal: &Principal, op: &str, _args: &Value) -> Value {
    super::commands::unknown_op(op)
}
