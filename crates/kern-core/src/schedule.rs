//! Schedule parsing and next-run computation (ARCHITECTURE.md §13, SPEC.md §13).
//!
//! Three kinds, exactly one per agent (`SPEC.md §9` validation enforces it):
//! - `every`: a fixed interval. The first run of a fresh agent fires
//!   immediately, then every interval; missed runs collapse (never catch-up
//!   storms — `next_after(now)`).
//! - `cron`: a standard 5-field expression (`minute hour dom month dow`; the
//!   6/7-field seconds/year forms and `@shorthands` are also accepted, see
//!   `config::validate_cron`). Computed in `timezone` (UTC default).
//! - `at`: an absolute RFC3339 instant, UTC. One shot; after it passes, no
//!   more runs.
//!
//! The cron crate's iterator is timezone-generic, so timezone handling lives
//! here (convert `from` into the schedule's zone, compute, convert back) — the
//! `timezone` config field never leaks elsewhere.

use std::str::FromStr;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;

use crate::config::ScheduleConfig;
use crate::error::{ErrorCode, KernError, Result};

/// The compiled schedule of an agent.
#[derive(Debug, Clone)]
pub enum Schedule {
    Every(std::time::Duration),
    Cron {
        expr: String,
        // Boxed: `cron::Schedule` is a large owned structure (its variant
        // table); the box keeps the enum small for task storage.
        parsed: Box<cron::Schedule>,
        tz: Tz,
    },
    At(DateTime<Utc>),
}

impl Schedule {
    /// Compile the config's schedule block. `None` when the agent has no
    /// schedule (one-shot agents are never auto-run).
    pub fn from_config(cfg: &ScheduleConfig) -> Result<Option<Schedule>> {
        if cfg.every.is_none() && cfg.cron.is_none() && cfg.at.is_none() {
            return Ok(None);
        }
        if let Some(every) = &cfg.every {
            return Ok(Some(Schedule::Every(every.as_std())));
        }
        if let Some(expr) = &cfg.cron {
            let tz = match &cfg.timezone {
                Some(name) => name.parse::<Tz>().map_err(|_| {
                    KernError::new(
                        ErrorCode::ConfigInvalid,
                        format!("schedule.timezone {name:?} is not a valid IANA timezone"),
                    )
                })?,
                None => Tz::UTC,
            };
            let normalized = normalize_cron(expr);
            let parsed = cron::Schedule::from_str(&normalized).map_err(|e| {
                KernError::new(
                    ErrorCode::ConfigInvalid,
                    format!("invalid cron expression {expr:?}: {e}"),
                )
            })?;
            return Ok(Some(Schedule::Cron {
                expr: expr.clone(),
                parsed: Box::new(parsed),
                tz,
            }));
        }
        if let Some(at) = &cfg.at {
            return Ok(Some(Schedule::At(*at)));
        }
        // Unreachable: validation guarantees exactly one kind. Defensive.
        Ok(None)
    }

    /// The next run time strictly after `from`, or `None` when the schedule
    /// has no future occurrences (expired `at`, impossible cron like
    /// `0 0 31 2 *`).
    pub fn next_after(&self, from: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Every(interval) => Some(from + chrono::Duration::from_std(*interval).ok()?),
            Schedule::Cron { parsed, tz, .. } => {
                let local_from = from.with_timezone(tz);
                parsed
                    .after(&local_from)
                    .next()
                    .map(|t| t.with_timezone(&Utc))
            }
            Schedule::At(at) => (*at > from).then_some(*at),
        }
    }

    /// Whether a freshly-created agent with no `next_run_at` yet should fire
    /// immediately (interval and missed `at` runs) or wait for its computed
    /// occurrence (cron, future `at`).
    pub fn first_occurrence(&self, now: DateTime<Utc>) -> Option<DateTime<Utc>> {
        match self {
            Schedule::Every(_) => Some(now),
            Schedule::At(at) => Some(if *at > now { *at } else { now }),
            Schedule::Cron { .. } => self.next_after(now),
        }
    }

    /// Whether the agent is scheduled to run again after `from` at all (used
    /// to decide `next_run_at` advance vs. retirement).
    pub fn has_next(&self, from: DateTime<Utc>) -> bool {
        self.next_after(from).is_some()
    }
}

/// Mirror of `config::validate_cron`'s normalization (5-field → seconds=0).
fn normalize_cron(expr: &str) -> String {
    if expr.trim_start().starts_with('@') {
        expr.to_string()
    } else {
        match expr.split_whitespace().count() {
            5 => format!("0 {expr}"),
            _ => expr.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::parse_agent_spec;

    fn schedule(yaml_schedule: &str) -> Schedule {
        let spec = parse_agent_spec(&format!(
            "version: 1\nname: s\nmodel:\n  provider: mock\n  model: test\ntools:\n  - noop\nschedule:\n{yaml_schedule}\n"
        ))
        .expect("schedule yaml must parse");
        Schedule::from_config(
            spec.schedule
                .as_ref()
                .expect("schedule block present in yaml"),
        )
        .expect("compile schedule")
        .expect("schedule present")
    }

    fn utc(s: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn every_advances_by_the_interval() {
        let s = schedule("  every: 30s\n");
        let from = utc("2026-08-14T12:00:00Z");
        assert_eq!(s.next_after(from).unwrap(), utc("2026-08-14T12:00:30Z"));
        // Strictly after: an occurrence AT `from` is not returned.
        assert_eq!(
            s.next_after(utc("2026-08-14T12:00:30Z")).unwrap(),
            utc("2026-08-14T12:01:00Z")
        );
        // First occurrence of a fresh agent fires immediately.
        assert_eq!(s.first_occurrence(from).unwrap(), from);
    }

    #[test]
    fn cron_midnight_crosses_day_boundaries() {
        let s = schedule("  cron: '0 0 * * *'\n");
        let from = utc("2026-08-14T23:30:00Z");
        assert_eq!(s.next_after(from).unwrap(), utc("2026-08-15T00:00:00Z"));
        // Exactly at midnight: the next occurrence is tomorrow's midnight.
        assert_eq!(
            s.next_after(utc("2026-08-15T00:00:00Z")).unwrap(),
            utc("2026-08-16T00:00:00Z")
        );
    }

    #[test]
    fn cron_9am_new_york_is_13_utc() {
        let s = schedule("  cron: '0 9 * * *'\n  timezone: America/New_York\n");
        // 2026-08-14 is EDT (UTC-4).
        let from = utc("2026-08-14T00:00:00Z");
        assert_eq!(s.next_after(from).unwrap(), utc("2026-08-14T13:00:00Z"));
    }

    #[test]
    fn cron_monthly_works_across_year_boundary() {
        let s = schedule("  cron: '0 0 1 1 *'\n"); // Jan 1, 00:00
        let from = utc("2026-01-01T00:00:00Z");
        assert_eq!(s.next_after(from).unwrap(), utc("2027-01-01T00:00:00Z"));
    }

    #[test]
    fn at_fires_once_then_expires() {
        // Config validation requires `at` to be in the future at parse time,
        // so the compiled schedule uses a far-future instant and expiry is
        // exercised through `next_after`.
        let s = schedule("  at: '2030-01-01T00:00:00Z'\n");
        let before = utc("2029-12-31T23:00:00Z");
        assert_eq!(s.next_after(before).unwrap(), utc("2030-01-01T00:00:00Z"));
        assert!(
            s.next_after(utc("2030-01-01T00:00:00Z")).is_none(),
            "one-shot"
        );
        // First occurrence of a fresh agent: future `at` waits for it.
        assert_eq!(
            s.first_occurrence(before).unwrap(),
            utc("2030-01-01T00:00:00Z")
        );
        // A missed `at` fires once late (the daemon was down).
        assert_eq!(
            s.first_occurrence(utc("2030-06-01T00:00:00Z")).unwrap(),
            utc("2030-06-01T00:00:00Z")
        );
    }

    #[test]
    fn six_field_cron_and_shorthands_are_accepted() {
        // 6-field seconds-first form.
        let s = schedule("  cron: '30 0 0 * * *'\n");
        let from = utc("2026-08-14T23:00:00Z");
        assert_eq!(s.next_after(from).unwrap(), utc("2026-08-15T00:00:30Z"));
        // @hourly shorthand.
        let s = schedule("  cron: '@hourly'\n");
        let from = utc("2026-08-14T12:30:00Z");
        assert_eq!(s.next_after(from).unwrap(), utc("2026-08-14T13:00:00Z"));
    }

    #[test]
    fn impossible_cron_has_no_next_run() {
        // Feb 30 never exists; the iterator must yield nothing, not loop.
        let s = schedule("  cron: '0 0 30 2 *'\n");
        assert!(s.next_after(utc("2026-01-01T00:00:00Z")).is_none());
        assert!(!s.has_next(utc("2026-01-01T00:00:00Z")));
    }
}
