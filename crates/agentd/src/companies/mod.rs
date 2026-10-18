// SPDX-License-Identifier: AGPL-3.0-only
//! Acting at companies for a person: agentd as a Personal Agent under the
//! Personal Agent Protocol draft 0.1, pinned at commit `95fcbf2`
//! (rfcs/0047). So far the module holds what the Personal Agent is built
//! and tested against: the protocol's wire constants ([`wire`]) and the fake
//! Company ([`mock`]), with the draft's JSON Schemas it holds itself to.
//!
//! Both are test scaffolding for now, compiled where the mock MCP servers
//! are — debug builds, or `internal-mocks` — and only with `oauth`, whose
//! `ring` verifies the assertions and DPoP Proofs the fake is sent, and
//! `tls`, since every URL the draft admits is HTTPS. No release binary
//! carries either.

pub mod mock;
pub mod wire;
