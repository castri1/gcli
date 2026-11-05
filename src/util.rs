//! Small dependency-free helpers: time, paths, text and encodings.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// Current Unix time in seconds (fractional).
pub fn now_secs() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0)
}

/// The command name this binary was invoked as (`gcli`, or a renamed copy), for hints.
pub fn program() -> String {
    std::env::args_os()
        .next()
        .and_then(|a| std::path::Path::new(&a).file_stem().map(|s| s.to_string_lossy().into_owned()))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "gcli".into())
}

/// The user's home directory, on every platform.
pub fn home_dir() -> Option<PathBuf> {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var).filter(|v| !v.is_empty()).map(PathBuf::from)
}

/// `path` with the home directory shown as `~`, safe to print.
pub fn display_path(path: &std::path::Path) -> String {
    sanitize(&match home_dir().and_then(|h| path.strip_prefix(h).ok().map(|rest| rest.to_path_buf())) {
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    })
}

// ---------------------------------------------------------------------------
// Dates (UTC only; civil-from-days from Howard Hinnant's date algorithms)
// ---------------------------------------------------------------------------

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let m = i64::from(m);
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Splits Unix seconds into UTC (year, month, day, hour, minute, second).
pub fn utc_parts(epoch: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = epoch.div_euclid(86_400);
    let secs = epoch.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, (secs / 3600) as u32, ((secs % 3600) / 60) as u32, (secs % 60) as u32)
}

/// `2026-10-19 20:25 UTC`
pub fn fmt_utc(epoch: f64) -> String {
    let (y, mo, d, h, mi, _) = utc_parts(epoch as i64);
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02} UTC")
}

/// Parses ISO 8601 / RFC 3339 timestamps: `2026-10-19T20:25:01Z`, `…01.123456789Z` (as Google
/// Cloud writes them), `…01UTC`, `…01+00:00`, `…01.000+0000`.
pub fn parse_rfc3339(s: &str) -> Option<f64> {
    let s = s.trim();
    let b = s.as_bytes();
    if b.len() < 19 || b[4] != b'-' || b[7] != b'-' || !matches!(b[10], b'T' | b't' | b' ') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<u32>().ok();
    let (y, mo, d) = (num(0..4)? as i64, num(5..7)?, num(8..10)?);
    let (h, mi, sec) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut rest = &s[19..];
    let mut frac = 0.0;
    if let Some(r) = rest.strip_prefix('.') {
        let digits = r.chars().take_while(char::is_ascii_digit).count();
        frac = format!("0.{}", &r[..digits]).parse().unwrap_or(0.0);
        rest = &r[digits..];
    }
    let offset = match rest {
        "" | "Z" | "z" | "UTC" => 0,
        _ => {
            let sign = match rest.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let digits: String = rest[1..].chars().filter(char::is_ascii_digit).collect();
            if digits.len() != 4 {
                return None;
            }
            let oh: i64 = digits[..2].parse().ok()?;
            let om: i64 = digits[2..].parse().ok()?;
            sign * (oh * 3600 + om * 60)
        }
    };
    let days = days_from_civil(y, mo, d);
    let secs = days * 86_400 + i64::from(h) * 3600 + i64::from(mi) * 60 + i64::from(sec) - offset;
    Some(secs as f64 + frac)
}

// ---------------------------------------------------------------------------
// Human formatting
// ---------------------------------------------------------------------------

/// `45s`, `2m 34s`, `1h 05m`
pub fn fmt_duration(secs: f64) -> String {
    let s = secs.max(0.0) as u64;
    match s {
        0..=59 => format!("{s}s"),
        60..=3599 => format!("{}m {:02}s", s / 60, s % 60),
        _ => format!("{}h {:02}m", s / 3600, (s % 3600) / 60),
    }
}

/// `just now`, `5m ago`, `3h ago`, `2d ago`
pub fn fmt_ago(epoch: f64) -> String {
    let s = (now_secs() - epoch).max(0.0) as u64;
    match s {
        0..=9 => "just now".into(),
        10..=59 => format!("{s}s ago"),
        60..=3599 => format!("{}m ago", s / 60),
        3600..=86_399 => format!("{}h ago", s / 3600),
        _ => format!("{}d ago", s / 86_400),
    }
}

/// Cuts `s` to at most `max` characters, ending in `…` when shortened.
pub fn ellipsize(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// Every whitespace-separated term of the (lower-cased) query appears in `name`.
pub fn matches_filter(name: &str, query_lower: &str) -> bool {
    let name = name.to_lowercase();
    query_lower.split_whitespace().all(|term| name.contains(term))
}

/// A log line that looks like an error, a failed test or a crash.
pub fn looks_like_error(line: &str) -> bool {
    const HINTS: [&str; 15] = [
        "error",
        "err!",
        "fail",
        "exception",
        "traceback",
        "panic",
        "fatal",
        "denied",
        "not found",
        "cannot ",
        "unable to",
        "exit status",
        "exit code",
        "✗",
        "✖",
    ];
    let lower = line.to_lowercase();
    HINTS.iter().any(|h| lower.contains(h))
        && !["0 error", "0 fail", "no error", "without error"].iter().any(|ok| lower.contains(ok))
}

/// Bidi overrides, zero-width and other format characters, line and paragraph separators, and
/// the Unicode tag block: invisible, and able to make text read differently from what it is (or
/// carry hidden instructions to an AI). Emoji variation selectors stay.
fn is_invisible(c: char) -> bool {
    matches!(
        c,
        '\u{00ad}'
            | '\u{061c}'
            | '\u{180e}'
            | '\u{200b}'..='\u{200f}'
            | '\u{2028}'..='\u{202e}'
            | '\u{2060}'..='\u{206f}'
            | '\u{feff}'
            | '\u{fff9}'..='\u{fffb}'
            | '\u{e0000}'..='\u{e007f}'
    )
}

/// Control characters (other than `\n` and `\t`) and invisible formatting characters.
pub fn is_unsafe(c: char) -> bool {
    (c.is_control() && c != '\n' && c != '\t') || is_invisible(c)
}

/// Removes terminal escape sequences, control characters other than `\n` and `\t`, and
/// invisible formatting characters, so text from Google Cloud (names, labels, logs, errors) or a
/// config file can neither drive the terminal nor disguise itself.
pub fn sanitize(s: &str) -> String {
    if !s.chars().any(is_unsafe) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\u{1b}' => match chars.peek() {
                // CSI: parameters until a final byte in @..~
                Some('[') => {
                    chars.next();
                    for c in chars.by_ref() {
                        if ('@'..='~').contains(&c) {
                            break;
                        }
                    }
                }
                // OSC, DCS, APC, PM, SOS: until BEL or ST
                Some(']' | 'P' | '_' | '^' | 'X') => {
                    chars.next();
                    while let Some(c) = chars.next() {
                        if c == '\u{7}' || c == '\u{9c}' || (c == '\u{1b}' && chars.peek() == Some(&'\\')) {
                            if c == '\u{1b}' {
                                chars.next();
                            }
                            break;
                        }
                    }
                }
                // Two-character sequences (ESC 7, ESC c, …)
                _ => {
                    chars.next();
                }
            },
            c if is_unsafe(c) => {}
            c => out.push(c),
        }
    }
    out
}

/// Sanitizes every string and object key of a JSON value in place (documents shown in the
/// terminal or handed to an AI agent).
pub fn sanitize_json(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::String(s) if s.chars().any(is_unsafe) => *s = sanitize(s),
        Value::Array(items) => items.iter_mut().for_each(sanitize_json),
        Value::Object(map) => {
            let entries = std::mem::take(map);
            for (k, mut v) in entries {
                sanitize_json(&mut v);
                map.insert(sanitize(&k), v);
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Encodings
// ---------------------------------------------------------------------------

pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 0xf) as usize] as char);
    }
    s
}

/// RFC 3986 percent-encoding of everything except unreserved characters.
pub fn uri_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn base64(input: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = match chunk.len() {
            3 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8 | u32::from(chunk[2]),
            2 => u32::from(chunk[0]) << 16 | u32::from(chunk[1]) << 8,
            _ => u32::from(chunk[0]) << 16,
        };
        out.push(T[(n >> 18 & 63) as usize] as char);
        out.push(T[(n >> 12 & 63) as usize] as char);
        out.push(if chunk.len() > 1 { T[(n >> 6 & 63) as usize] as char } else { '=' });
        out.push(if chunk.len() > 2 { T[(n & 63) as usize] as char } else { '=' });
    }
    out
}
