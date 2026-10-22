// SPDX-License-Identifier: AGPL-3.0-only
//! An `mcp.tool` step's payment (RFC 0049 §7.1), over the x402 v2 MCP
//! transport: the tool's error result carrying a `PaymentRequired` goes to
//! the loop, and once the payment is allowed the call is made again — the
//! same tool, arguments and `_meta`, the `PaymentPayload` beside them in
//! `_meta["x402/payment"]` — and its result's
//! `_meta["x402/payment-response"]` says what became of it.
//!
//! A paying call is never made an MCP task (RFC 0050): x402 binds a
//! payment to the call's own result, which a task would move to a
//! `tasks/result` the transport says nothing of. The call goes out without
//! the Tasks extension, so a server answers it inline or refuses it.
//!
//! The step's output is `{result, payment}`: the tool's result as an
//! `mcp.tool` step's output always is, and the payment — `null` when the
//! server asked for none. A model's turn never pays: its call of the same
//! tool reads the payment request as the tool's error (RFC 0049 D2).

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};

use super::{Answer, Asked, PaidCall};
use crate::mcp::x402 as wire;
use crate::payments::x402::Settlement;
use crate::runtime::reactor::Runtime;

/// An `mcp.tool` step's call, as made: what its paid retry makes again.
pub(crate) struct ToolCall {
    pub client: Arc<crate::mcp::client::McpClient>,
    pub server: String,
    pub tool: String,
    pub args: Value,
    /// The first call's `_meta` — the idempotency key, the attempt, the
    /// trace — which the paid call carries again.
    pub meta: Value,
    pub timeout: Duration,
    /// The call's span.
    pub trace: Option<crate::obs::trace::TraceContext>,
}

impl PaidCall for ToolCall {
    /// A payment request names this call when its resource is
    /// `mcp://tool/<the tool called>`: one that names another tool, or
    /// anything else, is never paid.
    fn is_resource(&self, url: &str) -> bool {
        url == self.shown()
    }

    /// The resource a payment for this call names: `mcp://tool/<tool>`.
    fn shown(&self) -> String {
        wire::tool_resource(&self.tool)
    }

    /// The tool quoted, as a run may template it; the server is a
    /// configured one's name.
    fn describe(&self) -> String {
        format!(
            "a call of the tool {} on the MCP server {}",
            crate::sec::render::quoted(&self.tool, super::SAID_SHOWN),
            self.server
        )
    }

    /// The paid call: the same call, `_meta["x402/payment"]` beside its
    /// `_meta`, never a task. Its result's `_meta["x402/payment-response"]`
    /// is the settlement; the payment request again is the server's word
    /// that it took none. An HTTP 4xx refuses the call; a call that never
    /// left was not paid; anything else may have reached the server.
    fn send_paid(&mut self, payload: &Value, _run: &str, _step: &str) -> Answer {
        use ::mcp::client::Reach;
        let start = crate::obs::otel::now_unix_nanos();
        let mut meta = self.meta.clone();
        meta[wire::PAYMENT] = payload.clone();
        let called = self.client.call_tool_called(
            &self.tool,
            Some(self.args.clone()),
            Some(meta),
            Some(self.timeout),
            false,
        );
        let answer = match called {
            Err(f) => {
                let why = format!("transport error: {}", f.error);
                match f.reach {
                    Reach::Unsent | Reach::Unsigned => Answer::Unanswered {
                        why,
                        may_have_sent: false,
                    },
                    Reach::Answered { status } if (400..500).contains(&status) => {
                        Answer::Answered {
                            output: json!({"result": null}),
                            settlement: None,
                            declined: Some(format!("the server refused the paid call ({why})")),
                            error: None,
                        }
                    }
                    _ => Answer::Unanswered {
                        why,
                        may_have_sent: true,
                    },
                }
            }
            Ok(::mcp::wire::Called::Task(_)) => Answer::Unanswered {
                why: "the server made the paid call a task, which x402 over MCP does not settle"
                    .into(),
                may_have_sent: true,
            },
            Ok(::mcp::wire::Called::Result(res)) => Answer::Answered {
                output: json!({"result": crate::runtime::worker::tool_result_value(&res)}),
                settlement: wire::response_of(&res).map(Settlement::from_value),
                declined: wire::required_of(&res).is_some().then(|| {
                    "the server answered the paid call with a payment request again, and settled nothing".into()
                }),
                error: res.is_error().then(|| res.text()),
            },
        };
        crate::obs::otel::mcp_span(
            self.trace.as_ref(),
            "tools/call",
            &self.tool,
            &self.server,
            matches!(
                &answer,
                Answer::Answered {
                    settlement: Some(Ok(s)),
                    error: None,
                    ..
                } if s.success
            ),
            start,
        );
        answer
    }
}

/// The first answer of an `mcp.tool` step that may pay, on its thread: a
/// payment request goes to the loop ([`Asked`]); anything else is the
/// step's, `{result, payment: null}`.
pub(crate) fn answered(
    res: ::mcp::wire::CallToolResult,
    call: ToolCall,
    run: String,
    step: String,
    start: u128,
    tx: &std::sync::mpsc::Sender<crate::runtime::events::Event>,
) {
    use crate::runtime::events::Event;
    crate::obs::otel::mcp_span(
        call.trace.as_ref(),
        "tools/call",
        &call.tool,
        &call.server,
        !res.is_error(),
        start,
    );
    let result = crate::runtime::worker::tool_result_value(&res);
    if let Some(raw) = wire::required_of(&res) {
        let _ = tx.send(Event::PaymentRequired(Box::new(Asked {
            run,
            step,
            required: Ok(raw),
            first: json!({"result": result}),
            call: Box::new(call),
        })));
        return;
    }
    let error = res.is_error().then(|| res.text());
    let _ = tx.send(Event::StepDone {
        run,
        step,
        output: json!({"result": result, "payment": null}),
        is_error: error.is_some(),
        error,
        tokens: 0,
    });
}

impl Runtime {
    /// Before a paying `mcp.tool` step calls anything: the payment checks
    /// every step makes ([`Runtime::payment_precheck`]), and a server whose
    /// calls are a person's own — a delegated server's (RFC 0044) — is
    /// paid from no wallet of agentd's.
    pub(crate) fn mcp_payment_precheck(
        &self,
        run_id: &str,
        step_id: &str,
        server: &str,
    ) -> Result<(), String> {
        #[cfg(feature = "xaa")]
        if self.is_delegated_server(server) {
            return Err(format!(
                "mcp.tool: {server} is a delegated server — its calls act for its person, and agentd pays for none of them (RFC 0049)"
            ));
        }
        let _ = server;
        self.payment_precheck(run_id, step_id)
    }
}
