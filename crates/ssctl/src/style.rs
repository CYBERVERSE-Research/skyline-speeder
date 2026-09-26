// SPDX-License-Identifier: GPL-2.0-only
// Copyright (c) 2026 CYBERVERSE LLC
//! Colours, symbols, bars and unit formatting for `ssctl`'s human output.
//!
//! Everything a view draws goes through a `Theme`, so that one decision --
//! is this a terminal, does it do colour, does it do UTF-8, how wide is it
//! -- is made once and cannot be forgotten in a single line somewhere.
use std::fmt::Write as _;

/// `--color`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum ColorChoice {
    /// Colour when stdout is a terminal and `NO_COLOR` is unset.
    Auto,
    Always,
    Never,
}

pub struct Theme {
    color: bool,
    unicode: bool,
    /// Usable width, already clamped to something a table can live in.
    pub width: usize,
}

const RESET: &str = "\x1b[0m";

impl Theme {
    pub fn detect(choice: ColorChoice) -> Self {
        let tty = unsafe { libc::isatty(libc::STDOUT_FILENO) } == 1;
        // https://no-color.org: any value at all, even empty, means off.
        let forbidden = std::env::var_os("NO_COLOR").is_some()
            || std::env::var("TERM").is_ok_and(|term| term == "dumb");
        let color = match choice {
            ColorChoice::Always => true,
            ColorChoice::Never => false,
            ColorChoice::Auto => tty && !forbidden,
        };
        Self {
            color,
            unicode: utf8_locale(),
            width: terminal_width(tty).clamp(60, 120),
        }
    }

    /// Whether stderr is a terminal. `--json` puts the sponsor line there
    /// rather than on stdout, and only when someone is looking: a cron job
    /// that pipes the JSON into a check would otherwise mail one line of
    /// sponsorship on every run, which is how a notice turns into something
    /// operators silence with `2>/dev/null`.
    pub fn stderr_is_terminal(&self) -> bool {
        unsafe { libc::isatty(libc::STDERR_FILENO) == 1 }
    }

    fn paint(&self, code: &str, text: &str) -> String {
        if self.color {
            format!("\x1b[{code}m{text}{RESET}")
        } else {
            text.to_owned()
        }
    }

    pub fn bold(&self, text: &str) -> String {
        self.paint("1", text)
    }
    pub fn dim(&self, text: &str) -> String {
        self.paint("2", text)
    }
    pub fn red(&self, text: &str) -> String {
        self.paint("31", text)
    }
    pub fn green(&self, text: &str) -> String {
        self.paint("32", text)
    }
    pub fn yellow(&self, text: &str) -> String {
        self.paint("33", text)
    }
    pub fn cyan(&self, text: &str) -> String {
        self.paint("36", text)
    }
    pub fn bold_cyan(&self, text: &str) -> String {
        self.paint("1;36", text)
    }
    pub fn bold_green(&self, text: &str) -> String {
        self.paint("1;32", text)
    }
    pub fn bold_red(&self, text: &str) -> String {
        self.paint("1;31", text)
    }
    pub fn bold_yellow(&self, text: &str) -> String {
        self.paint("1;33", text)
    }

    /// A state marker: green check, red cross, yellow warning, dim dash.
    pub fn mark(&self, state: Mark) -> String {
        let (symbol, ascii) = match state {
            Mark::Good => ("\u{2714}", "+"),
            Mark::Bad => ("\u{2718}", "x"),
            Mark::Warn => ("\u{26a0}", "!"),
            Mark::Off => ("\u{00b7}", "."),
            Mark::Info => ("\u{25b8}", ">"),
        };
        let glyph = if self.unicode { symbol } else { ascii };
        match state {
            Mark::Good => self.green(glyph),
            Mark::Bad => self.red(glyph),
            Mark::Warn => self.yellow(glyph),
            Mark::Off => self.dim(glyph),
            Mark::Info => self.cyan(glyph),
        }
    }

    /// The lamp next to the title: filled when accelerating.
    pub fn lamp(&self, on: bool) -> String {
        let glyph = if self.unicode {
            if on {
                "\u{25cf}"
            } else {
                "\u{25cb}"
            }
        } else if on {
            "*"
        } else {
            "o"
        };
        if on {
            self.bold_green(glyph)
        } else {
            self.dim(glyph)
        }
    }

    pub fn rule(&self) -> String {
        let glyph = if self.unicode { "\u{2500}" } else { "-" };
        self.dim(&glyph.repeat(self.width))
    }

    /// A section heading: bold, with the rest of the line ruled off so the
    /// eye can find where one block ends and the next begins.
    pub fn section(&self, title: &str) -> String {
        let glyph = if self.unicode { "\u{2500}" } else { "-" };
        let pad = self.width.saturating_sub(title.chars().count() + 3);
        format!(
            " {} {}",
            self.bold(title),
            self.dim(&glyph.repeat(pad.max(1)))
        )
    }

    /// A proportion bar, `width` cells wide. Coloured by `state` so that a
    /// bar meaning "good" and one meaning "trouble" never look alike.
    pub fn bar(&self, ratio: f64, width: usize, state: Mark) -> String {
        let ratio = ratio.clamp(0.0, 1.0);
        let filled = (ratio * width as f64).round() as usize;
        let (full, empty) = if self.unicode {
            ("\u{2588}", "\u{2591}")
        } else {
            ("#", ".")
        };
        let mut bar = String::new();
        let _ = write!(bar, "{}", full.repeat(filled));
        let _ = write!(bar, "{}", empty.repeat(width.saturating_sub(filled)));
        let painted = match state {
            Mark::Good => self.green(&bar),
            Mark::Bad => self.red(&bar),
            Mark::Warn => self.yellow(&bar),
            _ => self.cyan(&bar),
        };
        format!("{}{painted}{}", self.dim("["), self.dim("]"))
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Good,
    Bad,
    Warn,
    /// Deliberately not in use -- a module switched off, a feature not
    /// configured. Not a fault, so never red.
    Off,
    Info,
}

/// UTF-8 in the caller's locale. A terminal that cannot render box drawing
/// prints mojibake that is worse than the ASCII fallback.
fn utf8_locale() -> bool {
    for name in ["LC_ALL", "LC_CTYPE", "LANG"] {
        if let Ok(value) = std::env::var(name) {
            if value.is_empty() {
                continue;
            }
            let value = value.to_ascii_lowercase();
            return value.contains("utf-8") || value.contains("utf8");
        }
    }
    false
}

fn terminal_width(tty: bool) -> usize {
    if !tty {
        // Piped into a file or a pager: a fixed width keeps the output
        // diffable and stops a table from depending on who is watching.
        return 100;
    }
    let mut size: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut size) };
    if rc == 0 && size.ws_col > 0 {
        return usize::from(size.ws_col);
    }
    std::env::var("COLUMNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(100)
}

/// 1_204_551 -> "1,204,551". Grouped because these counters run to the
/// billions and an operator has to compare two of them at a glance.
pub fn count(value: u64) -> String {
    let digits = value.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (index, ch) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index) % 3 == 0 {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

/// Bytes in the units `ss` and `iostat` use: 1024-based, one decimal.
pub fn bytes(value: u64) -> String {
    const UNITS: [&str; 6] = ["B", "KiB", "MiB", "GiB", "TiB", "PiB"];
    scale(value as f64, 1024.0, &UNITS)
}

/// Bits per second, 1000-based -- the unit a link is sold in, and the one
/// `max_pacing_mbps` is written in.
pub fn bits_per_second(value: u64) -> String {
    const UNITS: [&str; 5] = ["b/s", "kb/s", "Mb/s", "Gb/s", "Tb/s"];
    scale(value as f64, 1000.0, &UNITS)
}

fn scale(mut value: f64, step: f64, units: &[&str]) -> String {
    let mut unit = 0;
    while value >= step && unit + 1 < units.len() {
        value /= step;
        unit += 1;
    }
    if unit == 0 {
        format!("{} {}", value.round() as u64, units[0])
    } else if value < 10.0 {
        format!("{value:.2} {}", units[unit])
    } else {
        format!("{value:.1} {}", units[unit])
    }
}

/// "3h 12m", "5d 2h", "42s" -- two units at most, which is all anyone reads.
pub fn duration(seconds: u64) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    let (hours, rest) = (rest / 3_600, rest % 3_600);
    let (minutes, secs) = (rest / 60, rest % 60);
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m {secs}s")
    } else {
        format!("{secs}s")
    }
}

/// Cuts `text` to `width` columns, marking that something was cut. Peer
/// addresses and guard notes are both arbitrarily long.
pub fn ellipsize(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_owned();
    }
    let keep = width.saturating_sub(1);
    let mut out: String = text.chars().take(keep).collect();
    out.push('\u{2026}');
    out
}

/// Breaks `text` into lines of at most `width` columns, on spaces.
///
/// Diagnostics wrap; they are never cut. A guard note or a capability
/// warning is the one thing on the screen the operator has to read in full,
/// and `ellipsize` on one of those hides the half that says what to do.
/// A single word longer than `width` (a path, a URL) is left over-long
/// rather than broken, because a broken path cannot be copied.
pub fn wrap(text: &str, width: usize) -> Vec<String> {
    let width = width.max(20);
    let mut lines = Vec::new();
    let mut line = String::new();
    for word in text.split_whitespace() {
        let extra = if line.is_empty() { 0 } else { 1 };
        if !line.is_empty() && line.chars().count() + extra + word.chars().count() > width {
            lines.push(std::mem::take(&mut line));
        }
        if !line.is_empty() {
            line.push(' ');
        }
        line.push_str(word);
    }
    if !line.is_empty() {
        lines.push(line);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

/// Pads to `width` columns, counting characters rather than bytes so an
/// IPv6 address or a UTF-8 note does not shift the column after it.
pub fn pad(text: &str, width: usize) -> String {
    let len = text.chars().count();
    if len >= width {
        text.to_owned()
    } else {
        format!("{text}{}", " ".repeat(width - len))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_are_grouped() {
        assert_eq!(count(0), "0");
        assert_eq!(count(999), "999");
        assert_eq!(count(1_000), "1,000");
        assert_eq!(count(1_204_551), "1,204,551");
    }

    #[test]
    fn units_scale_the_way_their_field_is_read() {
        assert_eq!(bytes(0), "0 B");
        assert_eq!(bytes(1536), "1.50 KiB");
        assert_eq!(bytes(1_288_490_188), "1.20 GiB");
        assert_eq!(bits_per_second(38_200_000), "38.2 Mb/s");
        assert_eq!(bits_per_second(611), "611 b/s");
    }

    #[test]
    fn durations_show_two_units() {
        assert_eq!(duration(42), "42s");
        assert_eq!(duration(605), "10m 5s");
        assert_eq!(duration(11_520), "3h 12m");
        assert_eq!(duration(439_200), "5d 2h");
    }

    /// A theme with colour off must emit no escape codes at all: its output
    /// lands in logs, in `script` captures and in this project's own
    /// installer log.
    #[test]
    fn plain_theme_emits_no_escapes() {
        let theme = Theme {
            color: false,
            unicode: false,
            width: 80,
        };
        let drawn = format!(
            "{}{}{}{}",
            theme.bold("t"),
            theme.mark(Mark::Good),
            theme.bar(0.5, 4, Mark::Good),
            theme.section("S")
        );
        assert!(!drawn.contains('\x1b'), "{drawn}");
        assert!(drawn.contains("##.."), "{drawn}");
    }

    #[test]
    fn bars_fill_in_proportion_and_clamp() {
        let theme = Theme {
            color: false,
            unicode: true,
            width: 80,
        };
        assert_eq!(theme.bar(0.0, 4, Mark::Good), "[░░░░]");
        assert_eq!(theme.bar(0.5, 4, Mark::Good), "[██░░]");
        assert_eq!(theme.bar(2.0, 4, Mark::Good), "[████]");
        assert_eq!(theme.bar(-1.0, 4, Mark::Good), "[░░░░]");
    }

    #[test]
    fn diagnostics_wrap_and_never_lose_their_tail() {
        let text = "TC observer unavailable: BPF object build/bpf/skyline_tc.bpf.o does not \
                    exist; run make bpf";
        let lines = wrap(text, 40);
        assert!(lines.len() > 1);
        assert!(
            lines.iter().all(|line| line.chars().count() <= 40),
            "{lines:?}"
        );
        assert_eq!(lines.join(" "), text);
        // A path longer than the width stays in one piece: a broken path
        // cannot be pasted into a shell.
        let long = wrap("see /a/very/long/path/that/exceeds/the/width/entirely", 20);
        assert_eq!(long.len(), 2);
        assert_eq!(long[1], "/a/very/long/path/that/exceeds/the/width/entirely");
    }

    #[test]
    fn wide_text_is_cut_and_padded_by_column_not_byte() {
        assert_eq!(pad("ab", 4), "ab  ");
        assert_eq!(pad("abcdef", 4), "abcdef");
        assert_eq!(ellipsize("abcdef", 4), "abc\u{2026}");
        assert_eq!(ellipsize("[2001:db8::2]:60000", 30), "[2001:db8::2]:60000");
    }
}
