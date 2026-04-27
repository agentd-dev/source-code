//! `call_mcp_tool` and `read_mcp_resource` node handlers.
//!
//! Both handlers share a single [`McpRegistryRef`] — the map from
//! server name to `(client, allowlist)` pair. At request time each
//! handler reads `node.server` and resolves through the registry;
//! the request-time allowlist check uses the target server's
//! allowlist, not a global one.

use serde_json::{Value, json};

use crate::engine::{ExecutionContext, HandlerRegistry, NodeHandler, NodeOutcome};
use crate::error::{Error, Result};
use crate::mcp::registry::McpRegistryRef;
use crate::workflow::{Node, NodeKind};

/// Register both MCP handlers against the registry. The `registry`
/// handle is shared across handlers and across reloads — server
/// respawn / allowlist swap happens inside the handles, not here.
pub fn register(registry: &mut HandlerRegistry, mcp: McpRegistryRef) {
    registry.register(
        "call_mcp_tool",
        Box::new(CallMcpToolHandler { mcp: mcp.clone() }),
    );
    registry.register(
        "read_mcp_resource",
        Box::new(ReadMcpResourceHandler { mcp }),
    );
}

// ---------------------------------------------------------------------------
// call_mcp_tool
// ---------------------------------------------------------------------------

pub struct CallMcpToolHandler {
    mcp: McpRegistryRef,
}

impl NodeHandler for CallMcpToolHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::CallMcpTool {
            tool,
            args_from,
            server,
        } = &node.kind
        else {
            return Err(mismatch(node, "call_mcp_tool"));
        };

        let handle = self.mcp.resolve(server.as_deref())?;

        if !handle.allowlist.tool_allowed(tool) {
            return Err(Error::Policy(format!(
                "mcp tool `{tool}` denied by allowlist on server `{}`",
                handle.name
            )));
        }

        // Dry-run: surface what *would* have happened without
        // touching the server. Keeps CI / preview runs hermetic.
        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({
                    "tool": tool,
                    "server": handle.name,
                    "dry_run": true,
                }),
                branch: None,
            });
        }

        let arguments = match args_from {
            Some(path) => ctx.resolve_path(path).cloned().unwrap_or(Value::Null),
            None => Value::Null,
        };

        let result = {
            let client: &dyn crate::mcp::client::McpClient = handle.client.as_ref();
            client.call_tool(tool, arguments)?
        };
        Ok(NodeOutcome::Continue {
            value: json!({
                "tool": tool,
                "server": handle.name,
                "content": result.content,
                "is_error": result.is_error,
                "structured": result.structured_content,
            }),
            branch: if result.is_error {
                Some("error".into())
            } else {
                None
            },
        })
    }
}

// ---------------------------------------------------------------------------
// read_mcp_resource
// ---------------------------------------------------------------------------

pub struct ReadMcpResourceHandler {
    mcp: McpRegistryRef,
}

impl NodeHandler for ReadMcpResourceHandler {
    fn handle(&self, node: &Node, ctx: &mut ExecutionContext) -> Result<NodeOutcome> {
        let NodeKind::ReadMcpResource {
            resource_from,
            server,
        } = &node.kind
        else {
            return Err(mismatch(node, "read_mcp_resource"));
        };

        let handle = self.mcp.resolve(server.as_deref())?;

        // Resolve the URI from context.
        let uri_val = ctx
            .resolve_path(resource_from)
            .cloned()
            .ok_or_else(|| Error::Tool {
                tool: "read_mcp_resource".into(),
                reason: format!(
                    "resource_from `{resource_from}` is not set in the execution context"
                ),
            })?;
        let Value::String(uri) = uri_val else {
            return Err(Error::Tool {
                tool: "read_mcp_resource".into(),
                reason: format!("resource_from `{resource_from}` must resolve to a string"),
            });
        };

        if !handle.allowlist.resource_allowed(&uri) {
            return Err(Error::Policy(format!(
                "mcp resource `{uri}` denied by allowlist on server `{}`",
                handle.name
            )));
        }

        if ctx.dry_run {
            return Ok(NodeOutcome::Continue {
                value: json!({ "uri": uri, "server": handle.name, "dry_run": true }),
                branch: None,
            });
        }

        let result = {
            let client: &dyn crate::mcp::client::McpClient = handle.client.as_ref();
            client.read_resource(&uri)?
        };
        Ok(NodeOutcome::Continue {
            value: json!({
                "uri": uri,
                "server": handle.name,
                "contents": result.contents,
            }),
            branch: None,
        })
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn mismatch(node: &Node, expected: &str) -> Error {
    Error::Tool {
        tool: expected.into(),
        reason: format!(
            "handler for `{expected}` received node `{}` of kind `{}`",
            node.id,
            node.kind.name()
        ),
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::context::{RunOptions, TriggerMeta};
    use crate::mcp::allowlist::{McpAllowlist, ReloadableMcpAllowlist};
    use crate::mcp::client::{McpClient, MockMcpClient, ReloadableMcpClient};
    use crate::mcp::protocol::{ResourcesReadResult, ToolsCallResult};
    use crate::mcp::registry::{McpRegistry, McpServerHandle};
    use std::sync::Arc;

    fn mk_registry(mock: Arc<MockMcpClient>, allowlist: McpAllowlist) -> McpRegistryRef {
        let client: Box<dyn McpClient> = Box::new(CloneHandle(mock));
        let handle = Arc::new(McpServerHandle {
            name: "default".into(),
            client: Arc::new(ReloadableMcpClient::new(client)),
            allowlist: Arc::new(ReloadableMcpAllowlist::new(allowlist)),
        });
        Arc::new(McpRegistry::new(vec![handle]))
    }

    /// MockMcpClient is `!Send` through its internal state; wrapping
    /// in Arc + this thin passthrough lets the test boxes move across
    /// the registry boundary without changing the real client surface.
    struct CloneHandle(Arc<MockMcpClient>);
    impl McpClient for CloneHandle {
        fn call_tool(&self, name: &str, arguments: serde_json::Value) -> Result<ToolsCallResult> {
            self.0.call_tool(name, arguments)
        }
        fn read_resource(&self, uri: &str) -> Result<ResourcesReadResult> {
            self.0.read_resource(uri)
        }
    }

    fn ctx(input: Value) -> ExecutionContext {
        ExecutionContext::new(
            "e",
            "w",
            "s",
            TriggerMeta::manual(input),
            &RunOptions::default(),
        )
    }

    fn call_node(tool: &str, args_from: Option<&str>) -> Node {
        call_node_on(tool, args_from, None)
    }

    fn call_node_on(tool: &str, args_from: Option<&str>, server: Option<&str>) -> Node {
        Node {
            id: "call".into(),
            kind: NodeKind::CallMcpTool {
                tool: tool.into(),
                args_from: args_from.map(Into::into),
                server: server.map(Into::into),
            },
        }
    }

    fn read_node(resource_from: &str) -> Node {
        Node {
            id: "read".into(),
            kind: NodeKind::ReadMcpResource {
                resource_from: resource_from.into(),
                server: None,
            },
        }
    }

    #[test]
    fn tool_call_dispatches_args_from_context() {
        let mock = Arc::new(MockMcpClient::new());
        mock.enqueue_tool(ToolsCallResult {
            content: vec![json!({"type":"text","text":"done"})],
            is_error: false,
            structured_content: None,
        });
        let h = CallMcpToolHandler {
            mcp: mk_registry(mock.clone(), McpAllowlist::allow_all()),
        };
        let mut c = ctx(json!({ "payload": { "page_id": 42, "comment": "nit" } }));
        let out = h
            .handle(
                &call_node("comment_on_page", Some("trigger.payload")),
                &mut c,
            )
            .unwrap();
        assert_eq!(mock.tool_calls().len(), 1);
        assert_eq!(
            mock.tool_calls()[0].1,
            json!({ "page_id": 42, "comment": "nit" })
        );
        match out {
            NodeOutcome::Continue { value, branch } => {
                assert!(branch.is_none());
                assert_eq!(value["tool"], "comment_on_page");
                assert_eq!(value["is_error"], false);
                assert_eq!(value["content"][0]["text"], "done");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tool_call_denied_by_allowlist_errors() {
        let mock = Arc::new(MockMcpClient::new());
        let h = CallMcpToolHandler {
            mcp: mk_registry(
                mock.clone(),
                McpAllowlist {
                    allowed_tools: vec!["only_me".into()],
                    ..Default::default()
                },
            ),
        };
        let mut c = ctx(json!({}));
        let err = h.handle(&call_node("not_me", None), &mut c).unwrap_err();
        assert!(format!("{err}").contains("denied by allowlist"));
        assert!(mock.tool_calls().is_empty(), "client must not be called");
    }

    #[test]
    fn tool_is_error_routes_to_error_branch() {
        let mock = Arc::new(MockMcpClient::new());
        mock.enqueue_tool(ToolsCallResult {
            content: vec![],
            is_error: true,
            structured_content: None,
        });
        let h = CallMcpToolHandler {
            mcp: mk_registry(mock, McpAllowlist::allow_all()),
        };
        let mut c = ctx(json!({}));
        let out = h.handle(&call_node("x", None), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { branch, .. } => {
                assert_eq!(branch.as_deref(), Some("error"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn tool_call_dry_run_skips_client() {
        let mock = Arc::new(MockMcpClient::new());
        let h = CallMcpToolHandler {
            mcp: mk_registry(mock.clone(), McpAllowlist::allow_all()),
        };
        let mut c = ctx(json!({}));
        c.dry_run = true;
        let out = h.handle(&call_node("any", None), &mut c).unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["dry_run"], true);
            }
            _ => panic!(),
        }
        assert!(mock.tool_calls().is_empty());
    }

    #[test]
    fn read_resource_happy_path() {
        let mock = Arc::new(MockMcpClient::new());
        mock.enqueue_resource(ResourcesReadResult {
            contents: vec![json!({"uri":"docs://pages/42","text":"# hi"})],
        });
        let h = ReadMcpResourceHandler {
            mcp: mk_registry(
                mock,
                McpAllowlist {
                    allowed_resource_patterns: vec!["docs://pages/*".into()],
                    ..Default::default()
                },
            ),
        };
        let mut c = ctx(json!({ "resource_uri": "docs://pages/42" }));
        let out = h
            .handle(&read_node("trigger.resource_uri"), &mut c)
            .unwrap();
        match out {
            NodeOutcome::Continue { value, .. } => {
                assert_eq!(value["uri"], "docs://pages/42");
                assert_eq!(value["contents"][0]["text"], "# hi");
            }
            _ => panic!(),
        }
    }

    #[test]
    fn read_resource_denied_by_pattern() {
        let mock = Arc::new(MockMcpClient::new());
        let h = ReadMcpResourceHandler {
            mcp: mk_registry(
                mock,
                McpAllowlist {
                    allowed_resource_patterns: vec!["docs://pages/*".into()],
                    ..Default::default()
                },
            ),
        };
        let mut c = ctx(json!({ "resource_uri": "secrets://everything" }));
        let err = h
            .handle(&read_node("trigger.resource_uri"), &mut c)
            .unwrap_err();
        assert!(format!("{err}").contains("denied by allowlist"));
    }

    #[test]
    fn read_resource_missing_uri_errors() {
        let mock = Arc::new(MockMcpClient::new());
        let h = ReadMcpResourceHandler {
            mcp: mk_registry(mock, McpAllowlist::allow_all()),
        };
        let mut c = ctx(json!({}));
        let err = h.handle(&read_node("trigger.nope"), &mut c).unwrap_err();
        assert!(format!("{err}").contains("not set"));
    }
}
