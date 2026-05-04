//! Fixture-driven test harness (RFC §22).
//!
//! Lets workflow authors drop a self-contained fixture under any
//! `tests/fixtures/<name>/` directory and exercise the full
//! runtime against it in one line:
//!
//! ```ignore
//! #[test]
//! fn my_workflow() {
//!     agentd::testing::run_fixture("tests/fixtures/review").assert_pass();
//! }
//! ```
//!
//! A fixture directory carries two files:
//!
//! - `workflow.toml` — the workflow under test (ordinary shape).
//! - `fixture.toml` — trigger payload, mocks, and expectations.
//!
//! Mocks are **pre-loaded canned responses**: the fixture declares
//! them once; the [`FixtureRunner`] hands them to mock intelligence
//! / MCP clients so side-effect-free reasoning steps are replayable
//! without a real backend.

pub mod fixture;
pub mod runner;

pub use fixture::{Expected, Fixture, FixtureMocks, FixtureTrigger};
pub use runner::{FixtureResult, FixtureRunner, FixtureStatus, discover_fixtures, run_fixture};
