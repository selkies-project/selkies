/*
 * This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! A suite's checks, recorded as they are made.
//!
//! A test that drives one session through several steps keeps going past a check that did not
//! hold, so one run says everything that is wrong, and fails once at the end. Each check prints
//! `PASS` or `FAIL`, its block and its name on a line of its own, the format the run's driver
//! lists failures from.

use std::fmt::Display;

/// The checks of one block of a suite.
pub struct Checks {
    block: String,
    made: usize,
    failed: Vec<String>,
}

impl Checks {
    /// An empty record for the block named `block`.
    pub fn new(block: &str) -> Checks {
        Checks {
            block: block.to_owned(),
            made: 0,
            failed: Vec::new(),
        }
    }

    /// Record the check `name`, which holds when `ok`; `detail` is what was observed, kept to one
    /// line.
    pub fn check(&mut self, name: &str, ok: bool, detail: impl Display) {
        self.made += 1;
        let detail: String = detail
            .to_string()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(300)
            .collect();
        let verdict = if ok { "PASS" } else { "FAIL" };
        println!("{verdict}  [{}] {name}  {detail}", self.block);
        if !ok {
            self.failed.push(format!("{name}: {detail}"));
        }
    }

    /// Record that the check `name` could not be observed here, and why. Not a failure: the
    /// behavior is unproven on this host, and the run's report lists it.
    pub fn skip(&self, name: &str, reason: impl Display) {
        println!("SKIP  [{}] {name}  {reason}", self.block);
    }

    /// Print the count and fail the test when a check did not hold, with `context` (the server's
    /// log) after the list.
    ///
    /// # Panics
    ///
    /// When any recorded check failed.
    pub fn finish(self, context: &str) {
        println!(
            "[{}] {}/{} passed",
            self.block,
            self.made - self.failed.len(),
            self.made
        );
        assert!(
            self.failed.is_empty(),
            "[{}] {} of {} checks failed:\n  {}\n--- server log ---\n{context}",
            self.block,
            self.failed.len(),
            self.made,
            self.failed.join("\n  ")
        );
    }
}
