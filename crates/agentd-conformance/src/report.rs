// SPDX-License-Identifier: Apache-2.0
//! Aggregate result of a conformance run, with text + JSON renderings.

use crate::{Category, Outcome};
use serde_json::{Value, json};

/// One check's record in the report.
pub struct Record {
    pub id: &'static str,
    pub category: Category,
    pub desc: &'static str,
    pub outcome: Outcome,
}

/// The whole run.
pub struct Report {
    pub records: Vec<Record>,
}

impl Report {
    pub fn new(records: Vec<Record>) -> Report {
        Report { records }
    }

    pub fn passed(&self) -> usize {
        self.records.iter().filter(|r| r.outcome.passed).count()
    }

    pub fn failed(&self) -> usize {
        self.records.len() - self.passed()
    }

    pub fn all_passed(&self) -> bool {
        self.records.iter().all(|r| r.outcome.passed)
    }

    /// A human-readable report grouped by family, failures called out.
    pub fn render_text(&self) -> String {
        let mut out = String::new();
        let mut cat: Option<Category> = None;
        for r in &self.records {
            if cat != Some(r.category) {
                cat = Some(r.category);
                out.push_str(&format!("\n  {}\n", r.category.as_str()));
            }
            let mark = if r.outcome.passed { "  ok  " } else { "FAIL  " };
            out.push_str(&format!("    [{mark}] {}\n", r.id));
            if !r.outcome.detail.is_empty() {
                out.push_str(&format!("            {}\n", r.outcome.detail));
            }
        }
        out.push_str(&format!(
            "\n  {} passed, {} failed, {} total\n",
            self.passed(),
            self.failed(),
            self.records.len()
        ));
        out
    }

    /// Machine-readable conformance record.
    pub fn to_json(&self) -> Value {
        json!({
            "passed": self.passed(),
            "failed": self.failed(),
            "total": self.records.len(),
            "all_passed": self.all_passed(),
            "checks": self.records.iter().map(|r| json!({
                "id": r.id,
                "category": r.category.as_str(),
                "desc": r.desc,
                "passed": r.outcome.passed,
                "detail": r.outcome.detail,
            })).collect::<Vec<_>>(),
        })
    }
}
