// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
//! Error reports: ask whether to send one, and send it only on a "y".
//!
//! Runtime errors worth a report reach `ssctl` in two ways: a command that
//! fails in a way that is not the operator's doing (`enable` refused, the
//! daemon down after a failure, a reply that does not decode), and what
//! skyline-speederd recorded in `DAEMON_REPORTS_DIR` when it stopped on an
//! error or panicked -- it has no terminal to ask on. Either way the question
//! is asked once, on the terminal, after the command's own output; nothing is
//! sent on any answer but y, and with no terminal (a script, `--json`, cron)
//! nothing is asked or sent. `SKYLINE_FEEDBACK=yes` answers y in advance,
//! `SKYLINE_FEEDBACK=no` never asks. Daemon reports are removed once
//! answered, so each is asked about once.
//!
//! install.sh asks the same way about a failed install ("error reports"
//! there): keep the two in step -- what a report holds, what `scrub` removes,
//! the question. Reports go to a Sentry project whose DSN is sealed; see
//! `SEALED` and `infra/seal-feedback-dsn.py`.

use crate::style::{wrap, Theme};
use serde_json::{json, Value};
use skyline_common::{DaemonReport, DAEMON_REPORTS_DIR};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::os::fd::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

/// The DSN, sealed: ChaCha20 (RFC 8439) under `SEAL_KEY` and `SEAL_NONCE`.
/// The key is public, as it must be for this program to open the DSN, so
/// sealing only keeps it out of plain text -- out of secret scanners, a grep
/// for the ingest host and `strings` -- and that is all a DSN, which can only
/// submit events, needs. `infra/seal-feedback-dsn.py` writes this file and
/// install.sh's `FEEDBACK_SEALED`; the tests below check the two agree.
/// Empty: reports are off, and nothing asks.
const SEALED: &str = include_str!("feedback-dsn.sealed");
const SEAL_KEY: [u8; 32] = [
    0x9a, 0x50, 0x5f, 0xc6, 0x47, 0x34, 0xe2, 0xf2, 0xc8, 0x01, 0xa1, 0xc0, 0xe3, 0xd0, 0x31, 0xc5,
    0xff, 0x2f, 0x56, 0x62, 0xcd, 0x1b, 0xa5, 0x16, 0x24, 0xc7, 0x5d, 0xac, 0x95, 0x55, 0xde, 0x1a,
];
const SEAL_NONCE: [u8; 12] = [
    0xa4, 0x82, 0xac, 0xf4, 0x80, 0xd0, 0x92, 0x8b, 0x3c, 0x37, 0xc7, 0x30,
];

/// How long the question waits for an answer before it counts as a no.
const ANSWER_TIMEOUT_S: i32 = 300;
/// The daemon's log lines a report carries.
const JOURNAL_LINES: usize = 40;

fn quarter_round(x: &mut [u32; 16], a: usize, b: usize, c: usize, d: usize) {
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(16);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(12);
    x[a] = x[a].wrapping_add(x[b]);
    x[d] = (x[d] ^ x[a]).rotate_left(8);
    x[c] = x[c].wrapping_add(x[d]);
    x[b] = (x[b] ^ x[c]).rotate_left(7);
}

/// ChaCha20 over `data`, the block counter from 1 as in RFC 8439 2.4; it
/// seals and opens alike.
fn chacha20(key: &[u8; 32], nonce: &[u8; 12], data: &[u8]) -> Vec<u8> {
    let word = |bytes: &[u8]| u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let mut state = [0u32; 16];
    state[..4].copy_from_slice(&[0x6170_7865, 0x3320_646e, 0x7962_2d32, 0x6b20_6574]);
    for i in 0..8 {
        state[4 + i] = word(&key[i * 4..]);
    }
    for i in 0..3 {
        state[13 + i] = word(&nonce[i * 4..]);
    }
    let mut out = Vec::with_capacity(data.len());
    for (block, chunk) in data.chunks(64).enumerate() {
        state[12] = block as u32 + 1;
        let mut x = state;
        for _ in 0..10 {
            quarter_round(&mut x, 0, 4, 8, 12);
            quarter_round(&mut x, 1, 5, 9, 13);
            quarter_round(&mut x, 2, 6, 10, 14);
            quarter_round(&mut x, 3, 7, 11, 15);
            quarter_round(&mut x, 0, 5, 10, 15);
            quarter_round(&mut x, 1, 6, 11, 12);
            quarter_round(&mut x, 2, 7, 8, 13);
            quarter_round(&mut x, 3, 4, 9, 14);
        }
        let stream: Vec<u8> = (0..16)
            .flat_map(|i| x[i].wrapping_add(state[i]).to_le_bytes())
            .collect();
        out.extend(chunk.iter().zip(stream).map(|(byte, key)| byte ^ key));
    }
    out
}

fn from_hex(text: &str) -> Option<Vec<u8>> {
    if text.len() % 2 != 0 {
        return None;
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(text.get(i..i + 2)?, 16).ok())
        .collect()
}

/// Where reports go: the DSN's public key and its envelope endpoint.
#[derive(Debug, PartialEq, Eq)]
struct Dsn {
    public_key: String,
    endpoint: String,
}

impl Dsn {
    /// `https://<32 hex>@<host>[:<port>][/<path>]/<project id>`, the form the
    /// seal script accepts (and install.sh's feedback_dsn).
    fn parse(dsn: &str) -> Option<Self> {
        let rest = dsn.strip_prefix("https://")?;
        let (key, rest) = rest.split_once('@')?;
        let (location, project) = rest.rsplit_once('/')?;
        let (host, path) = location
            .split_once('/')
            .map_or((location, ""), |(h, p)| (h, p));
        let is_hex = |c: char| c.is_ascii_digit() || ('a'..='f').contains(&c);
        let host_ok = !host.is_empty()
            && host.split_once(':').map_or(true, |(_, port)| {
                !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit())
            })
            && host
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':'));
        let path_ok = path.is_empty()
            || path.split('/').all(|part| {
                !part.is_empty()
                    && part
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
            });
        let valid = key.len() == 32
            && key.chars().all(is_hex)
            && host_ok
            && path_ok
            && !project.is_empty()
            && project.bytes().all(|b| b.is_ascii_digit());
        let path = if path.is_empty() {
            String::new()
        } else {
            format!("/{path}")
        };
        valid.then(|| Self {
            public_key: key.to_owned(),
            endpoint: format!("https://{host}{path}/api/{project}/envelope/"),
        })
    }

    fn sealed() -> Option<Self> {
        let sealed = from_hex(SEALED.trim()).filter(|bytes| !bytes.is_empty())?;
        Self::parse(std::str::from_utf8(&chacha20(&SEAL_KEY, &SEAL_NONCE, &sealed)).ok()?)
    }
}

/// Whether to ask, and whether there is anyone to ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    Ask,
    Send,
    Never,
}

pub struct Feedback {
    mode: Mode,
    dsn: Option<Dsn>,
}

impl Feedback {
    /// Root only: the daemon's reports are root's (0700), and a command that
    /// fails for anyone else fails on the socket's permissions, which is no
    /// bug. `--json` is for scripts, which are never asked.
    pub fn detect(json: bool) -> Self {
        let answer = std::env::var("SKYLINE_FEEDBACK").unwrap_or_default();
        let terminal = unsafe {
            libc::isatty(libc::STDIN_FILENO) == 1 && libc::isatty(libc::STDERR_FILENO) == 1
        };
        let root = unsafe { libc::geteuid() } == 0;
        let mode = match answer.to_ascii_lowercase().as_str() {
            _ if !root => Mode::Never,
            "1" | "y" | "yes" | "true" => Mode::Send,
            "0" | "n" | "no" | "false" | "never" => Mode::Never,
            _ if json || !terminal => Mode::Never,
            _ => Mode::Ask,
        };
        let dsn = if mode == Mode::Never {
            None
        } else {
            Dsn::sealed()
        };
        Self { mode, dsn }
    }

    /// After a command that succeeded, or failed for a reason of its own:
    /// what the daemon recorded, if anything.
    pub fn offer_recorded(&self, theme: &Theme) {
        if self.dsn.is_some() {
            self.offer(theme, None);
        }
    }

    /// After `kind` failed with `message`; the daemon's records go along.
    pub fn offer_error(&self, theme: &Theme, kind: &'static str, message: &str) {
        if self.dsn.is_some() {
            self.offer(theme, Some((kind, message)));
        }
    }

    /// After the daemon could not be reached: worth a report only when the
    /// service manager gave up on it, or it recorded why it stopped -- not
    /// when somebody stopped it, and not while it is (re)starting: a daemon
    /// in a restart loop has recorded why by then.
    pub fn offer_unreachable(&self, theme: &Theme, message: &str) {
        if self.dsn.is_none() {
            return;
        }
        if service_state() == "failed" || !recorded().is_empty() {
            self.offer(theme, Some(("daemon unreachable", message)));
        } else {
            self.offer(theme, None);
        }
    }

    fn offer(&self, theme: &Theme, error: Option<(&'static str, &str)>) {
        let Some(dsn) = &self.dsn else { return };
        let recorded = recorded();
        if error.is_none() && recorded.is_empty() {
            return;
        }
        let host = Host::detect();
        let journal = scrub(&journal_tail());
        // Each event with the daemon's report it came from, removed once
        // that event is sent -- or once the answer is no.
        let mut events: Vec<(Value, Option<&Path>)> = Vec::new();
        if let Some((kind, message)) = error {
            events.push((event(&host, kind, &scrub(message), None, &journal), None));
        }
        for (path, report) in &recorded {
            let kind = if report.kind == "panicked" {
                "daemon panicked"
            } else {
                "daemon exited"
            };
            let event = event(&host, kind, &scrub(&report.message), Some(report), &journal);
            events.push((event, Some(path)));
        }
        let mut tty = match self.mode {
            Mode::Ask => match OpenOptions::new().read(true).write(true).open("/dev/tty") {
                Ok(tty) => Some(tty),
                Err(_) => return,
            },
            Mode::Send => None,
            Mode::Never => return,
        };
        if self.mode == Mode::Ask {
            let stopped = match recorded.len() {
                1 => "1 error skyline-speederd stopped on".to_owned(),
                n => format!("{n} errors skyline-speederd stopped on"),
            };
            let what = match (error.is_some(), recorded.is_empty()) {
                (true, true) => "the error above".to_owned(),
                (true, false) => format!("the error above and {stopped}"),
                (false, _) => stopped,
            };
            say(
                &mut tty,
                &format!(
                    "\n {} Send an error report to the Skyline Speeder developers.\n",
                    theme.bold("Help us fix this?")
                ),
            );
            let explained = format!(
                "It holds {what}, the versions, the distribution, the kernel and the daemon's \
                 last log lines, with IP, MAC and e-mail addresses, the host name and user names \
                 removed. Nothing is sent unless you answer y."
            );
            for line in wrap(&explained, 76) {
                say(&mut tty, &format!("   {line}\n"));
            }
            loop {
                say(&mut tty, "   Send it? [y/N, v to view it first] ");
                let answer = tty.as_ref().and_then(read_answer).unwrap_or_default();
                match answer.trim() {
                    "v" | "V" => {
                        let shown: Vec<Value> =
                            events.iter().map(|(event, _)| event.clone()).collect();
                        say(&mut tty, &render(&shown));
                    }
                    "y" | "Y" | "yes" | "YES" | "Yes" => break,
                    _ => {
                        say(&mut tty, "   not sent\n");
                        for (path, _) in &recorded {
                            let _ = fs::remove_file(path);
                        }
                        return;
                    }
                }
            }
        }
        let mut sent = Vec::new();
        for (event, path) in &events {
            if let Err(error) = send(dsn, event) {
                say(
                    &mut tty,
                    &format!(
                        "   {} the report could not be sent: {error}\n",
                        theme.yellow("warn")
                    ),
                );
                break;
            }
            if let Some(path) = path {
                let _ = fs::remove_file(path);
            }
            let id = event["event_id"].as_str().unwrap_or_default();
            sent.push(id.get(..8).unwrap_or(id).to_owned());
        }
        if !sent.is_empty() {
            say(
                &mut tty,
                &format!(
                    "   {}  sent (report {}), thank you\n",
                    theme.green("ok"),
                    sent.join(", ")
                ),
            );
        }
    }
}

/// To the terminal asked on, or with nobody asked, stderr.
fn say(tty: &mut Option<File>, text: &str) {
    match tty {
        Some(tty) => {
            let _ = tty.write_all(text.as_bytes());
        }
        None => eprint!("{text}"),
    }
}

/// One line from the terminal, or None after `ANSWER_TIMEOUT_S` or at its end.
fn read_answer(tty: &File) -> Option<String> {
    let mut poll = libc::pollfd {
        fd: tty.as_raw_fd(),
        events: libc::POLLIN,
        revents: 0,
    };
    if unsafe { libc::poll(&mut poll, 1, ANSWER_TIMEOUT_S * 1000) } != 1 {
        return None;
    }
    let mut line = String::new();
    BufReader::new(tty).read_line(&mut line).ok()?;
    Some(line)
}

/// The daemon's reports, oldest first; unreadable ones are left alone.
fn recorded() -> Vec<(PathBuf, DaemonReport)> {
    let Ok(entries) = fs::read_dir(DAEMON_REPORTS_DIR) else {
        return Vec::new();
    };
    let mut found: Vec<(PathBuf, DaemonReport)> = entries
        .filter_map(|entry| entry.ok().map(|entry| entry.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let report = serde_json::from_slice(&fs::read(&path).ok()?).ok()?;
            Some((path, report))
        })
        .collect();
    found.sort_by_key(|(_, report)| report.first_seen);
    found
}

fn init_system() -> &'static str {
    if Path::new("/run/systemd/system").exists() {
        "systemd"
    } else if Path::new("/run/openrc").exists() {
        "openrc"
    } else {
        "none"
    }
}

fn output(program: &str, args: &[&str]) -> String {
    Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .output()
        .map(|out| String::from_utf8_lossy(&out.stdout).into_owned())
        .unwrap_or_default()
}

/// skyline-speederd as the service manager sees it, in install.sh's words
/// (svc_state): active, activating, failed or inactive.
fn service_state() -> &'static str {
    match init_system() {
        "systemd" => match output("systemctl", &["is-active", "skyline-speederd.service"]).trim() {
            "active" | "reloading" | "refreshing" => "active",
            "activating" | "deactivating" => "activating",
            "failed" => "failed",
            _ => "inactive",
        },
        "openrc" => {
            let status = output("rc-service", &["skyline-speederd", "status"]);
            if status.contains("status: started") {
                "active"
            } else if status.contains("status: starting") || status.contains("status: stopping") {
                "activating"
            } else if status.contains("status: crashed") {
                "failed"
            } else {
                "inactive"
            }
        }
        _ => "inactive",
    }
}

/// The daemon's last log lines: the journal, or under OpenRC the syslog
/// lines its logger tags (packaging/openrc/skyline-speederd).
fn journal_tail() -> String {
    let lines = JOURNAL_LINES.to_string();
    match init_system() {
        "systemd" => output(
            "journalctl",
            &[
                "-u",
                "skyline-speederd.service",
                "-n",
                &lines,
                "--no-pager",
                "-o",
                "cat",
            ],
        ),
        "openrc" => {
            let log = fs::read_to_string("/var/log/messages").unwrap_or_default();
            let tagged: Vec<&str> = log
                .lines()
                .filter(|line| line.contains("skyline-speederd"))
                .collect();
            tagged[tagged.len().saturating_sub(JOURNAL_LINES)..].join("\n")
        }
        _ => String::new(),
    }
}

/// What a report says about the host, none of it naming it.
struct Host {
    os_id: String,
    os_version: String,
    os_name: String,
    kernel: String,
    arch: String,
    init: &'static str,
    service: &'static str,
}

impl Host {
    fn detect() -> Self {
        let release = fs::read_to_string("/etc/os-release").unwrap_or_default();
        let field = |name: &str| {
            release
                .lines()
                .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
                .map(|value| value.trim().trim_matches('"').to_owned())
                .unwrap_or_default()
        };
        let mut uname: libc::utsname = unsafe { std::mem::zeroed() };
        let (kernel, arch) = if unsafe { libc::uname(&mut uname) } == 0 {
            let text = |chars: &[libc::c_char]| unsafe {
                std::ffi::CStr::from_ptr(chars.as_ptr())
                    .to_string_lossy()
                    .into_owned()
            };
            (text(&uname.release), text(&uname.machine))
        } else {
            (String::new(), String::new())
        };
        let os_id = field("ID");
        let os_name = Some(field("PRETTY_NAME"))
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| os_id.clone());
        Self {
            os_id,
            os_version: field("VERSION_ID"),
            os_name,
            kernel,
            arch,
            init: init_system(),
            service: service_state(),
        }
    }
}

fn event_id() -> String {
    let mut bytes = [0u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut random| random.read_exact(&mut bytes))
        .is_err()
    {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or(0);
        bytes = (nanos ^ u128::from(std::process::id())).to_le_bytes();
    }
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// One Sentry event. `message` is scrubbed by the caller, `journal` too.
fn event(
    host: &Host,
    kind: &str,
    message: &str,
    report: Option<&DaemonReport>,
    journal: &str,
) -> Value {
    let version = report.map_or(env!("CARGO_PKG_VERSION"), |report| report.version.as_str());
    let libc = if cfg!(target_env = "musl") {
        "musl"
    } else {
        "glibc"
    };
    let first_line: String = message
        .lines()
        .next()
        .unwrap_or_default()
        .chars()
        .map(|c| if c.is_ascii_digit() { '0' } else { c })
        .collect();
    let mut extra = json!({ "journal_tail": journal });
    if let Some(report) = report {
        extra["count"] = json!(report.count);
        extra["first_seen"] = json!(report.first_seen);
        extra["last_seen"] = json!(report.last_seen);
    }
    json!({
        "event_id": event_id(),
        "timestamp": SystemTime::now().duration_since(UNIX_EPOCH).map(|e| e.as_secs()).unwrap_or(0),
        "platform": "native",
        "level": if kind == "daemon panicked" { "fatal" } else { "error" },
        "logger": if report.is_some() { "skyline-speederd" } else { "ssctl" },
        "release": format!("skyline-speeder@{version}"),
        "environment": "production",
        "tags": {
            "component": if report.is_some() { "skyline-speederd" } else { "ssctl" },
            "kind": kind,
            "os": host.os_id,
            "os_version": host.os_version,
            "kernel": host.kernel,
            "arch": host.arch,
            "libc": libc,
            "init": host.init,
            "service": host.service,
        },
        "exception": { "values": [{ "type": kind, "value": message }] },
        "contexts": { "os": { "name": host.os_name, "kernel_version": host.kernel } },
        // Grouped by what failed and what it said, numbers aside.
        "fingerprint": ["ssctl", kind, first_line],
        "extra": extra,
    })
}

/// What `v` shows: every field of every event, as lines.
fn render(events: &[Value]) -> String {
    let mut out = String::new();
    for event in events {
        let text = |value: &Value| value.as_str().unwrap_or_default().to_owned();
        let exception = &event["exception"]["values"][0];
        out.push_str(&format!(
            "   error    {}: {}\n",
            text(&exception["type"]),
            text(&exception["value"])
        ));
        out.push_str(&format!("   release  {}\n", text(&event["release"])));
        let tags: Vec<String> = event["tags"]
            .as_object()
            .map(|tags| {
                tags.iter()
                    .map(|(name, value)| format!("{name}={}", text(value)))
                    .collect()
            })
            .unwrap_or_default();
        out.push_str(&format!("   tags     {}\n", tags.join(" ")));
        for name in ["count", "first_seen", "last_seen"] {
            if let Some(value) = event["extra"].get(name) {
                out.push_str(&format!("   {name:<8} {value}\n"));
            }
        }
        let journal = text(&event["extra"]["journal_tail"]);
        if !journal.trim().is_empty() {
            out.push_str("   log      the daemon's last log lines:\n");
            for line in journal.lines() {
                out.push_str(&format!("     | {line}\n"));
            }
        }
    }
    out
}

/// The envelope as a quoted value in a curl configuration, which takes \\,
/// \" and \n.
fn curl_quoted(text: &str) -> String {
    text.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

/// Everything that names the project -- the URL, the key -- goes in a curl
/// configuration on curl's stdin (-K -), not on a command line `ps` shows; -q
/// first, so no ~/.curlrc takes part. curl, not an HTTP client in this
/// binary: install.sh puts curl on every host, and ssctl, run as root, links
/// no TLS stack.
fn send(dsn: &Dsn, event: &Value) -> Result<(), String> {
    let envelope = format!(
        "{}\n{}\n{}\n",
        json!({ "event_id": event["event_id"] }),
        json!({ "type": "event" }),
        event
    );
    let config = format!(
        "url = \"{}\"\nheader = \"X-Sentry-Auth: Sentry sentry_version=7, sentry_client=ssctl/{}, \
         sentry_key={}\"\nheader = \"Content-Type: application/x-sentry-envelope\"\n\
         data-binary = \"{}\"\n",
        dsn.endpoint,
        env!("CARGO_PKG_VERSION"),
        dsn.public_key,
        curl_quoted(&envelope)
    );
    let mut curl = Command::new("curl")
        .args([
            "-q",
            "-fsS",
            "--proto",
            "=https",
            "--tlsv1.2",
            "--max-time",
            "20",
        ])
        .args(["-o", "/dev/null", "-K", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| format!("cannot run curl: {error}"))?;
    if let Some(mut stdin) = curl.stdin.take() {
        stdin
            .write_all(config.as_bytes())
            .map_err(|error| format!("cannot talk to curl: {error}"))?;
    }
    let out = curl
        .wait_with_output()
        .map_err(|error| format!("curl: {error}"))?;
    if out.status.success() {
        Ok(())
    } else {
        let said = String::from_utf8_lossy(&out.stderr).trim().to_owned();
        Err(if said.is_empty() {
            format!("curl exited with {}", out.status)
        } else {
            said
        })
    }
}

// --- scrubbing ----------------------------------------------------------------
// What names this host, its network or its people goes before anything is
// shown or sent: IPv4 and IPv6 addresses, MAC addresses, e-mail addresses,
// credentials in URLs, tokens, values of token=/secret=/password= pairs, home
// directories, the host name and the invoking user. The same list as
// install.sh's feedback_scrub; it errs on the side of removing too much (a
// version with four parts reads as an address).

/// `text` with every `[start, end)` range replaced by its marker; ranges come
/// sorted and do not overlap.
fn splice(text: &str, ranges: &[(usize, usize, &str)]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for &(start, end, marker) in ranges {
        out.push_str(&text[at..start]);
        out.push_str(marker);
        at = end;
    }
    out.push_str(&text[at..]);
    out
}

fn is_word(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// `https://user:password@host` keeps `https://` and `@host`.
fn scrub_credentials(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut from = 0;
    while let Some(found) = text[from..].find("://") {
        let start = from + found + 3;
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'@' || b == b'/' || b.is_ascii_whitespace())
            .map_or(bytes.len(), |n| start + n);
        if end > start && bytes.get(end) == Some(&b'@') {
            ranges.push((start, end, "<credentials>"));
        }
        from = end.max(start);
    }
    splice(text, &ranges)
}

fn scrub_emails(text: &str) -> String {
    let bytes = text.as_bytes();
    let local = |b: u8| b.is_ascii_alphanumeric() || b"._%+-".contains(&b);
    let domain = |b: u8| b.is_ascii_alphanumeric() || b".-".contains(&b);
    let mut ranges = Vec::new();
    let mut last_end = 0;
    for (at, _) in text.match_indices('@') {
        if at < last_end {
            continue;
        }
        let start = (last_end..at)
            .rev()
            .take_while(|&i| local(bytes[i]))
            .last()
            .unwrap_or(at);
        let mut end = (at + 1..bytes.len())
            .take_while(|&i| domain(bytes[i]))
            .last()
            .map_or(at + 1, |i| i + 1);
        while end > at + 1 && matches!(bytes[end - 1], b'.' | b'-') {
            end -= 1;
        }
        let host = &text[at + 1..end];
        let tld = host.rsplit_once('.').map_or("", |(_, tld)| tld);
        if start < at && tld.len() >= 2 && tld.bytes().all(|b| b.is_ascii_alphabetic()) {
            ranges.push((start, end, "<email>"));
            last_end = end;
        }
    }
    splice(text, &ranges)
}

fn scrub_tokens(text: &str) -> String {
    const PREFIXES: [&str; 9] = [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "sntrys_",
        "sntryu_",
        "sntrs_",
    ];
    // An organization token is sntrys_<base64>_<secret>, and standard base64
    // holds + / =: a body of word characters alone stopped at the first of
    // them and left the secret after it in the report.
    let body_byte = |b: u8| is_word(b) || b"+/=-".contains(&b);
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let prefix = PREFIXES
            .iter()
            .find(|prefix| text[i..].starts_with(*prefix) && (i == 0 || !is_word(bytes[i - 1])));
        if let Some(prefix) = prefix {
            let body = bytes[i + prefix.len()..]
                .iter()
                .take_while(|&&b| body_byte(b))
                .count();
            if body >= 16 {
                ranges.push((i, i + prefix.len() + body, "<token>"));
                i += prefix.len() + body;
                continue;
            }
        }
        i += 1;
    }
    splice(text, &ranges)
}

/// The value of `token=...`, `secret: ...`, `password=...` and the like.
fn scrub_secret_values(text: &str) -> String {
    let bytes = text.as_bytes();
    let lower = text.to_ascii_lowercase();
    let mut ranges = Vec::new();
    for (at, _) in text.match_indices(|c: char| c == '=' || c == ':') {
        let named = ["token", "secret", "password", "passwd"]
            .iter()
            .any(|name| lower[..at].ends_with(name));
        let end = bytes[at + 1..]
            .iter()
            .position(|b| b.is_ascii_whitespace())
            .map_or(bytes.len(), |n| at + 1 + n);
        if named && end > at + 1 && ranges.last().map_or(true, |&(_, last, _)| at >= last) {
            ranges.push((at + 1, end, "<redacted>"));
        }
    }
    splice(text, &ranges)
}

/// Maximal runs of bytes `in_run` accepts, as `(start, end)`.
fn runs(bytes: &[u8], in_run: impl Fn(u8) -> bool) -> Vec<(usize, usize)> {
    let mut found = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        if in_run(bytes[i]) {
            let start = i;
            while i < bytes.len() && in_run(bytes[i]) {
                i += 1;
            }
            found.push((start, i));
        } else {
            i += 1;
        }
    }
    found
}

fn scrub_ipv4(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    for (start, end) in runs(bytes, |b| b.is_ascii_digit() || b == b'.') {
        // The parts between dots, with where each starts.
        let mut parts = Vec::new();
        let mut at = start;
        for part in text[start..end].split('.') {
            parts.push((at, part.len()));
            at += part.len() + 1;
        }
        let mut i = 0;
        while i + 4 <= parts.len() {
            if parts[i..i + 4]
                .iter()
                .all(|&(_, len)| (1..=3).contains(&len))
            {
                let (first, _) = parts[i];
                let (last, len) = parts[i + 3];
                ranges.push((first, last + len, "<ipv4>"));
                i += 4;
            } else {
                i += 1;
            }
        }
    }
    splice(text, &ranges)
}

/// IPv6 addresses, full or with `::`, and MAC addresses, colon or dash
/// separated. A run counts only between non-word characters, so `std::io`
/// and `12:34:56` stay.
fn scrub_ipv6_and_mac(text: &str) -> String {
    let bytes = text.as_bytes();
    let bounded = |start: usize, end: usize| {
        (start == 0 || !is_word(bytes[start - 1])) && (end == bytes.len() || !is_word(bytes[end]))
    };
    let mut ranges = Vec::new();
    for (start, mut end) in runs(bytes, |b| b.is_ascii_hexdigit() || b == b':') {
        if end - start > 2 && bytes[end - 1] == b':' && bytes[end - 2] != b':' {
            end -= 1;
        }
        let run = &text[start..end];
        let groups: Vec<&str> = run.split(':').collect();
        let full = groups.len() >= 4 && groups.iter().all(|g| (1..=4).contains(&g.len()));
        let compressed = run.contains("::") && !run.contains(":::");
        if bounded(start, end) && (full || compressed) {
            ranges.push((start, end, if full { "<ipv6-or-mac>" } else { "<ipv6>" }));
        }
    }
    let text = splice(text, &ranges);
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    for (start, end) in runs(bytes, |b| b.is_ascii_hexdigit() || b == b'-') {
        let run = &text[start..end];
        let groups: Vec<&str> = run.split('-').collect();
        if groups.len() == 6 && groups.iter().all(|g| g.len() == 2) {
            ranges.push((start, end, "<mac>"));
        }
    }
    splice(&text, &ranges)
}

fn scrub_homes(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut ranges = Vec::new();
    for (at, found) in text.match_indices("/home/") {
        let start = at + found.len();
        let end = bytes[start..]
            .iter()
            .position(|&b| b == b'/' || b.is_ascii_whitespace())
            .map_or(bytes.len(), |n| start + n);
        if end > start && ranges.last().map_or(true, |&(_, last, _)| start >= last) {
            ranges.push((start, end, "<user>"));
        }
    }
    splice(text, &ranges)
}

/// `scrub` with the host and user names given, for the tests.
fn scrub_names(text: &str, hosts: &[String], users: &[String]) -> String {
    let mut text = scrub_credentials(text);
    for pass in [
        scrub_emails,
        scrub_tokens,
        scrub_secret_values,
        scrub_ipv4,
        scrub_ipv6_and_mac,
        scrub_homes,
    ] {
        text = pass(&text);
    }
    for name in hosts.iter().filter(|name| name.len() >= 3) {
        text = text.replace(name.as_str(), "<host>");
    }
    for name in users
        .iter()
        .filter(|name| name.len() >= 3 && *name != "root")
    {
        text = text.replace(name.as_str(), "<user>");
    }
    text
}

fn scrub(text: &str) -> String {
    let host = fs::read_to_string("/proc/sys/kernel/hostname").unwrap_or_default();
    let host = host.trim();
    let short = host.split('.').next().unwrap_or_default();
    let users: Vec<String> = ["SUDO_USER", "LOGNAME"]
        .iter()
        .filter_map(|name| std::env::var(name).ok())
        .collect();
    scrub_names(text, &[host.to_owned(), short.to_owned()], &users)
}

#[cfg(test)]
mod tests {
    use super::*;

    const INSTALL_SH: &str = include_str!("../../../install.sh");
    /// `https://0123456789abcdef0123456789abcdef@o0.ingest.example.invalid/42`
    /// sealed by infra/seal-feedback-dsn.py under the key above.
    const SAMPLE_SEALED: &str = "0b3e66e80afc3e718e477e2ac6e1d0ae4025b48c06f923fa1b7c46ec680f6b82\
                                 4e6d0d89fad7c4a1c1dd45709de931e97524ebc85fb43e8ccddc0a42bf3692ef\
                                 a744001c92";

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }

    fn shell_value(name: &str) -> &'static str {
        INSTALL_SH
            .lines()
            .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
            .unwrap_or_else(|| panic!("install.sh has no {name}= line"))
    }

    #[test]
    fn chacha20_matches_rfc_8439() {
        // Section 2.4.2.
        let key: [u8; 32] = std::array::from_fn(|i| i as u8);
        let nonce = [0, 0, 0, 0, 0, 0, 0, 0x4a, 0, 0, 0, 0];
        let plain = b"Ladies and Gentlemen of the class of '99: If I could offer you only one \
                      tip for the future, sunscreen would be it.";
        assert_eq!(
            hex(&chacha20(&key, &nonce, plain)),
            "6e2e359a2568f98041ba0728dd0d6981e97e7aec1d4360c20a27afccfd9fae0b\
             f91b65c5524733ab8f593dabcd62b3571639d624e65152ab8f530c359f0861d8\
             07ca0dbf500d6a6156a38e088a22b65e52bc514d16ccf806818ce91ab7793736\
             5af90bbf74a35be6b40b8eedf2785e42874d"
        );
    }

    #[test]
    fn install_sh_seals_with_the_same_key_and_the_same_dsn() {
        assert_eq!(shell_value("FEEDBACK_SEAL_KEY"), hex(&SEAL_KEY));
        assert_eq!(shell_value("FEEDBACK_SEAL_NONCE"), hex(&SEAL_NONCE));
        assert_eq!(
            shell_value("FEEDBACK_SEALED"),
            SEALED.trim(),
            "seal the DSN with infra/seal-feedback-dsn.py, which writes both copies"
        );
        if !SEALED.trim().is_empty() {
            assert!(
                Dsn::sealed().is_some(),
                "the sealed DSN does not open: seal it again"
            );
        }
    }

    #[test]
    fn a_dsn_sealed_by_the_seal_script_opens() {
        let opened = chacha20(&SEAL_KEY, &SEAL_NONCE, &from_hex(SAMPLE_SEALED).unwrap());
        assert_eq!(
            Dsn::parse(std::str::from_utf8(&opened).unwrap()),
            Some(Dsn {
                public_key: "0123456789abcdef0123456789abcdef".to_owned(),
                endpoint: "https://o0.ingest.example.invalid/api/42/envelope/".to_owned(),
            })
        );
    }

    #[test]
    fn install_sh_opens_it_the_same_way() {
        // The functions between install.sh's "error reports" heading and
        // on_interrupt, run by bash on the sample: the shell's ChaCha20 and
        // DSN parsing against these.
        let start = INSTALL_SH.find("# --- error reports").expect("section");
        let end = start + INSTALL_SH[start..].find("\non_interrupt() {").expect("end");
        let script = format!(
            "{}\nFEEDBACK_SEALED={SAMPLE_SEALED}\nfeedback_dsn && printf '%s %s' \"$FEEDBACK_PUBLIC\" \"$FEEDBACK_URL\"",
            &INSTALL_SH[start..end]
        );
        let out = Command::new("bash")
            .args(["-euc", &script])
            .output()
            .expect("bash");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            "0123456789abcdef0123456789abcdef https://o0.ingest.example.invalid/api/42/envelope/"
        );
    }

    #[test]
    fn install_sh_scrubs_like_ssctl() {
        // Both senders over one text: install.sh's sed missed sntrys_ and
        // sntryu_ (it matched sntr[ysu]_) while this file caught them, and
        // nothing compared the two. Same environment for both, so the host
        // and user names each takes from /proc and SUDO_USER / LOGNAME agree.
        let token = |prefix: &str, body: &str| format!("{prefix}{body}");
        let text = format!(
            "bind 192.0.2.20:8080 fe80::1%eth0 ::1 [::]:443 2001:db8:0:0:1:0:0:1 failed\n\
             use std::io::Write; anyhow::Result at 12:34:56 mac 52:54:00:12:34:56 aa-bb-cc-dd-ee-ff\n\
             git clone https://user:hunter2@github.com/x/y, mail ops+alerts@mail.example.org,\n\
             token=abc123 Secret: s3 PASSWORD=p cd {}\n\
             {} {} {}\n\
             org {} user {} not x{}\n",
            ["", "home", "alice", "src"].join("/"),
            token("ghp_", "ABCDEFGHIJKLMNOPQRSTUVWX1234"),
            token("github_pat_", "11ABCDEFG0123456789_abcdefghijklmnop"),
            token("sntrs_", "0123456789abcdef0123"),
            token("sntrys_", "eyJpYXQiOjE3MDAwMDAwMDAuMH0=_Zm9vYmFyYmF6cXV4cXV1eA/+ab"),
            token("sntryu_", &"0123456789abcdef".repeat(4)),
            token("ghp_", "ABCDEFGHIJKLMNOPQRSTUVWX1234"),
        );
        let start = INSTALL_SH.find("# --- error reports").expect("section");
        let end = start + INSTALL_SH[start..].find("\non_interrupt() {").expect("end");
        let script = format!("{}\nfeedback_scrub", &INSTALL_SH[start..end]);
        let mut child = Command::new("bash")
            .args(["-euc", &script])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("bash");
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(text.as_bytes())
            .expect("write");
        let out = child.wait_with_output().expect("bash");
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        let rust = scrub(&text);
        // $(...) in the shell drops the trailing newline.
        assert_eq!(
            String::from_utf8_lossy(&out.stdout),
            rust.trim_end_matches('\n')
        );
        assert!(!rust.contains("sntry") && !rust.contains("Zm9v") && !rust.contains("eyJp"));
        assert!(rust.contains("not xghp_"), "{rust}");
    }

    #[test]
    fn only_https_dsns_of_the_documented_form_are_taken() {
        let key = "0123456789abcdef0123456789abcdef";
        let parsed =
            Dsn::parse(&format!("https://{key}@sentry.example.org:9000/prefix/7")).unwrap();
        assert_eq!(
            parsed.endpoint,
            "https://sentry.example.org:9000/prefix/api/7/envelope/"
        );
        for bad in [
            format!("http://{key}@sentry.example.org/7"),
            format!("https://{key}@sentry.example.org/"),
            format!("https://{key}@sentry.example.org/x7"),
            format!("https://{}@sentry.example.org/7", &key[1..]),
            format!("https://{key}@sentry.example.org/a b/7"),
            format!("https://{key}@/7"),
            String::new(),
        ] {
            assert_eq!(Dsn::parse(&bad), None, "{bad}");
        }
    }

    #[test]
    fn scrub_removes_what_names_the_host_its_network_and_its_people() {
        let hosts = ["web-07.example.net".to_owned(), "web-07".to_owned()];
        let users = ["alice".to_owned(), "root".to_owned()];
        // The token and the home directory put together here: as literals in
        // a public tree they read as a leaked token and a personal path to
        // every scanner, this project's own included.
        let token = format!("ghp_{}", "ABCDEFGHIJKLMNOPQRSTUVWX1234");
        let home = ["", "home", "alice", "src"].join("/");
        let scrubbed = scrub_names(
            &format!(
                "Oct 27 10:00:01 web-07 skyline-speederd[1234]: bind 192.0.2.20:8080 fe80::1%eth0 \
                 ::1 [::]:443 2001:db8:0:0:1:0:0:1 failed\n\
                 use std::io::Write; anyhow::Result at 12:34:56 mac 52:54:00:12:34:56 \
                 aa-bb-cc-dd-ee-ff\n\
                 git clone https://user:hunter2@github.com/x/y, mail alice@example.com, \
                 token=abc123 {token} cd {home} as alice\n\
                 kernel 6.12.48+deb13-amd64 version 0.5.3 on web-07.example.net, root owns it"
            ),
            &hosts,
            &users,
        );
        assert_eq!(
            scrubbed,
            "Oct 27 10:00:01 <host> skyline-speederd[1234]: bind <ipv4>:8080 <ipv6>%eth0 \
             <ipv6> [<ipv6>]:443 <ipv6-or-mac> failed\n\
             use std::io::Write; anyhow::Result at 12:34:56 mac <ipv6-or-mac> \
             <mac>\n\
             git clone https://<credentials>@github.com/x/y, mail <email>, \
             token=<redacted> <token> cd /home/<user>/src as <user>\n\
             kernel 6.12.48+deb13-amd64 version 0.5.3 on <host>, root owns it"
        );
    }

    #[test]
    fn the_envelope_survives_curl_quoting() {
        let event = json!({ "event_id": "ab", "exception": { "values": [
            { "type": "enable failed", "value": "a \"quoted\" \\ path\nsecond line" }
        ] } });
        let envelope = format!("{}\n{}\n", json!({ "type": "event" }), event);
        // What curl's configuration parser makes of a quoted value.
        let quoted = curl_quoted(&envelope);
        let mut decoded = String::new();
        let mut chars = quoted.chars();
        while let Some(c) = chars.next() {
            match (c, c == '\\') {
                (_, true) => match chars.next() {
                    Some('n') => decoded.push('\n'),
                    Some(other) => decoded.push(other),
                    None => {}
                },
                (c, false) => decoded.push(c),
            }
        }
        assert_eq!(decoded, envelope);
        assert!(!quoted.contains('\n'));
    }
}
