//! Output and confirmation helpers.
//!
//! Contract: in `--json` mode, stdout carries exactly one JSON document per
//! invocation; everything human-oriented goes to stderr. Scripts can rely on
//! `lmm --json ... | jq` never seeing prose.

use std::io::{IsTerminal, Write};

use anyhow::{Result, bail};
use serde::Serialize;

#[derive(Clone, Copy)]
pub struct Out {
    pub json: bool,
    pub verbose: bool,
    pub yes: bool,
}

impl Out {
    /// Emit a result: JSON document in --json mode, otherwise the human view.
    pub fn emit<T: Serialize>(&self, value: &T, human: impl FnOnce()) -> Result<()> {
        if self.json {
            let mut stdout = std::io::stdout().lock();
            serde_json::to_writer_pretty(&mut stdout, value)?;
            writeln!(stdout)?;
        } else {
            human();
        }
        Ok(())
    }

    pub fn verbose(&self, msg: impl AsRef<str>) {
        if self.verbose {
            eprintln!("lmm: {}", msg.as_ref());
        }
    }

    /// Progress/info line for humans; kept off stdout in JSON mode.
    #[allow(dead_code)] // used from stage 4 onwards
    pub fn info(&self, msg: impl AsRef<str>) {
        if self.json {
            eprintln!("{}", msg.as_ref());
        } else {
            println!("{}", msg.as_ref());
        }
    }

    /// Ask before proceeding with a destructive action. Non-interactive runs
    /// must pass --yes: silently proceeding would defeat the point, silently
    /// aborting would break scripts in confusing ways.
    pub fn confirm(&self, prompt: &str) -> Result<bool> {
        if self.yes {
            return Ok(true);
        }
        let stdin = std::io::stdin();
        if !stdin.is_terminal() {
            bail!("confirmation required for: {prompt} (pass --yes to proceed non-interactively)");
        }
        eprint!("{prompt} [y/N] ");
        let mut line = String::new();
        stdin.read_line(&mut line)?;
        Ok(matches!(line.trim(), "y" | "Y" | "yes"))
    }
}

/// Minimal left-aligned column table for human output.
pub fn print_table(header: &[&str], rows: &[Vec<String>]) {
    let cols = header.len();
    let mut widths: Vec<usize> = header.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (i, cell) in row.iter().enumerate().take(cols) {
            widths[i] = widths[i].max(cell.chars().count());
        }
    }
    let print_row = |cells: &[String]| {
        let line = cells
            .iter()
            .enumerate()
            .map(|(i, c)| format!("{:<w$}", c, w = widths[i]))
            .collect::<Vec<_>>()
            .join("  ");
        println!("{}", line.trim_end());
    };
    print_row(&header.iter().map(|s| s.to_string()).collect::<Vec<_>>());
    print_row(&widths.iter().map(|w| "-".repeat(*w)).collect::<Vec<_>>());
    for row in rows {
        print_row(row);
    }
}

/// Human-readable timestamp (UTC) from unix seconds; avoids a date-time
/// dependency for one display concern.
pub fn fmt_time(unix: i64) -> String {
    // Civil-from-days algorithm (Howard Hinnant); valid across our range.
    let days = unix.div_euclid(86_400);
    let secs = unix.rem_euclid(86_400);
    let (h, m) = (secs / 3600, (secs % 3600) / 60);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fmt_time_known_values() {
        assert_eq!(fmt_time(0), "1970-01-01 00:00");
        assert_eq!(fmt_time(1_752_537_600), "2025-07-15 00:00");
    }
}
