// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
//! The errors skyline-speederd stops on, recorded in `DAEMON_REPORTS_DIR`.
//!
//! The daemon runs with no terminal and never sends a report itself: the next
//! `ssctl` run on a terminal asks whether to send what is recorded here, and
//! removes it either way (`crates/ssctl/src/feedback.rs`). Recording is local
//! only, best effort, and never what stops the daemon: a failure to write is
//! said on stderr and otherwise ignored.

use skyline_common::{DaemonReport, DAEMON_REPORTS_DIR, DAEMON_REPORTS_MAX};
use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

/// Records `kind` with `message`, adding to the report of the same failure
/// when there is one.
pub fn record(kind: &str, message: &str) {
    if let Err(error) = record_in(Path::new(DAEMON_REPORTS_DIR), kind, message, now()) {
        eprintln!("could not record the error report in {DAEMON_REPORTS_DIR}: {error}");
    }
}

/// Records every panic, in whichever thread, before the default hook prints
/// it: a panicking guard or sampler thread does not stop the daemon, so
/// nothing else would ever say it happened.
pub fn record_panics() {
    let default = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        record("panicked", &info.to_string());
        default(info);
    }));
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

/// FNV-1a over the kind and the message with every digit dropped: the same
/// failure on every restart differs only in a pid, a port or a count.
fn fingerprint(kind: &str, message: &str) -> u64 {
    kind.bytes()
        .chain([0])
        .chain(message.bytes().filter(|byte| !byte.is_ascii_digit()))
        .fold(0xcbf2_9ce4_8422_2325, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        })
}

fn record_in(dir: &Path, kind: &str, message: &str, now: u64) -> std::io::Result<()> {
    DirBuilder::new().recursive(true).mode(0o700).create(dir)?;
    let path = dir.join(format!("{:016x}.json", fingerprint(kind, message)));
    let report = match fs::read(&path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<DaemonReport>(&bytes).ok())
    {
        Some(seen) => DaemonReport {
            last_seen: now,
            count: seen.count.saturating_add(1),
            ..seen
        },
        None => {
            let waiting = fs::read_dir(dir)?
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
                .count();
            if waiting >= DAEMON_REPORTS_MAX {
                return Ok(());
            }
            DaemonReport {
                version: env!("CARGO_PKG_VERSION").to_owned(),
                kind: kind.to_owned(),
                message: message.to_owned(),
                first_seen: now,
                last_seen: now,
                count: 1,
            }
        }
    };
    // A whole file or none: ssctl may be reading the directory right now.
    let staged = path.with_extension("tmp");
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&staged)?;
    file.write_all(&serde_json::to_vec(&report).map_err(std::io::Error::from)?)?;
    file.sync_all()?;
    fs::rename(&staged, &path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("skyline-reports-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    fn reports(dir: &Path) -> Vec<DaemonReport> {
        let mut found: Vec<DaemonReport> = fs::read_dir(dir)
            .unwrap()
            .map(|entry| serde_json::from_slice(&fs::read(entry.unwrap().path()).unwrap()).unwrap())
            .collect();
        found.sort_by_key(|report| report.first_seen);
        found
    }

    #[test]
    fn the_same_failure_on_every_restart_is_one_report() {
        let dir = scratch("same");
        record_in(
            &dir,
            "exited",
            "bind /run/skyline-speeder/speeder.sock: pid 41 holds it",
            10,
        )
        .unwrap();
        record_in(
            &dir,
            "exited",
            "bind /run/skyline-speeder/speeder.sock: pid 977 holds it",
            12,
        )
        .unwrap();
        record_in(
            &dir,
            "panicked",
            "bind /run/skyline-speeder/speeder.sock: pid 41 holds it",
            13,
        )
        .unwrap();
        let found = reports(&dir);
        assert_eq!(found.len(), 2, "{found:?}");
        assert_eq!(
            (found[0].count, found[0].first_seen, found[0].last_seen),
            (2, 10, 12)
        );
        assert!(
            found[0].message.contains("pid 41"),
            "the first message is kept"
        );
        assert_eq!((found[1].kind.as_str(), found[1].count), ("panicked", 1));
        let mode = fs::metadata(&dir).unwrap().permissions();
        assert_eq!(
            std::os::unix::fs::PermissionsExt::mode(&mode) & 0o777,
            0o700
        );
        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_daemon_failing_a_new_way_each_time_stops_recording_at_the_cap() {
        let dir = scratch("cap");
        for i in 0..DAEMON_REPORTS_MAX + 5 {
            let letter = char::from(b'a' + i as u8);
            record_in(&dir, "exited", &format!("failure {letter}"), i as u64).unwrap();
        }
        assert_eq!(reports(&dir).len(), DAEMON_REPORTS_MAX);
        // One already recorded is still counted.
        record_in(&dir, "exited", "failure a", 99).unwrap();
        assert_eq!(reports(&dir)[0].count, 2);
        fs::remove_dir_all(&dir).unwrap();
    }
}
