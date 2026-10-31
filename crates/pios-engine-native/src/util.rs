//! Small helpers without dependencies: time formatting, base64, ids, sizes.

use std::time::{SystemTime, UNIX_EPOCH};

/// Nanoseconds since the epoch.
pub fn now_ns() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as i64)
}

/// RFC 3339 with nanoseconds in UTC, the format Docker uses for timestamps.
/// Zero means "never" and renders as Go's zero time, like dockerd.
pub fn rfc3339(ns: i64) -> String {
    if ns <= 0 {
        return "0001-01-01T00:00:00Z".into();
    }
    let secs = ns.div_euclid(1_000_000_000);
    let nanos = ns.rem_euclid(1_000_000_000);
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}.{nanos:09}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Days since 1970-01-01 to (year, month, day) (H. Hinnant's algorithm).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// Parse the subset of RFC 3339 that registries and image configs use.
pub fn parse_rfc3339(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() < 20 || b[4] != b'-' || b[7] != b'-' || (b[10] != b'T' && b[10] != b't') {
        return None;
    }
    let num = |r: std::ops::Range<usize>| s.get(r)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0..4)?, num(5..7)?, num(8..10)?);
    let (h, mi, se) = (num(11..13)?, num(14..16)?, num(17..19)?);
    let mut nanos = 0i64;
    let mut rest = &s[19..];
    if let Some(frac) = rest.strip_prefix('.') {
        let digits = frac.bytes().take_while(u8::is_ascii_digit).count();
        let mut f: String = frac[..digits].chars().take(9).collect();
        while f.len() < 9 {
            f.push('0');
        }
        nanos = f.parse().ok()?;
        rest = &frac[digits..];
    }
    let offset = match rest {
        "Z" | "z" => 0,
        o if o.len() == 6 => {
            let sign = if o.starts_with('-') { -1 } else { 1 };
            sign * (o[1..3].parse::<i64>().ok()? * 3600 + o[4..6].parse::<i64>().ok()? * 60)
        }
        _ => return None,
    };
    let days = days_from_civil(y, mo as u32, d as u32);
    Some(((days * 86_400 + h * 3600 + mi * 60 + se) - offset) * 1_000_000_000 + nanos)
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// "Up 5 minutes", "Exited (0) 2 hours ago": Docker's human durations.
pub fn human_duration(secs: i64) -> String {
    let secs = secs.max(0);
    match secs {
        0 => "Less than a second".into(),
        1 => "1 second".into(),
        s if s < 60 => format!("{s} seconds"),
        s if s < 120 => "About a minute".into(),
        s if s < 3600 => format!("{} minutes", s / 60),
        s if s < 7200 => "About an hour".into(),
        s if s < 172_800 => format!("{} hours", s / 3600),
        s if s < 1_209_600 => format!("{} days", s / 86_400),
        s if s < 5_184_000 => format!("{} weeks", s / 604_800),
        s if s < 63_072_000 => format!("{} months", s / 2_592_000),
        s => format!("{} years", s / 31_536_000),
    }
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn base64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let n = chunk.iter().enumerate().fold(0u32, |acc, (i, b)| acc | (*b as u32) << (16 - 8 * i));
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// Decodes standard and URL-safe base64, with or without padding
/// (X-Registry-Auth is URL-safe).
pub fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let (mut acc, mut bits) = (0u32, 0u32);
    for c in s.bytes().filter(|c| !c.is_ascii_whitespace()) {
        let v = match c {
            b'A'..=b'Z' => c - b'A',
            b'a'..=b'z' => c - b'a' + 26,
            b'0'..=b'9' => c - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// Lowercase hex encoding.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 64 hex characters of kernel randomness: container, exec and volume ids.
pub fn random_id() -> String {
    let mut buf = [0u8; 32];
    if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
        let _ = std::io::Read::read_exact(&mut f, &mut buf);
    } else {
        // Host tests on platforms without /dev/urandom: time based.
        buf[..16].copy_from_slice(&(now_ns() as u128).to_le_bytes());
    }
    hex(&buf)
}

/// Default container name: Docker style "adjective_noun".
pub fn random_name(id: &str) -> String {
    const ADJ: [&str; 16] = [
        "brave", "calm", "eager", "fond", "gentle", "happy", "jolly", "keen", "lucid", "merry", "nifty", "proud",
        "quirky", "sharp", "tender", "witty",
    ];
    const NOUN: [&str; 16] = [
        "babbage", "curie", "darwin", "euler", "fermi", "galois", "hopper", "kepler", "lovelace", "noether", "pascal",
        "raman", "shannon", "tesla", "turing", "wozniak",
    ];
    let n = u32::from_str_radix(&id[..8.min(id.len())], 16).unwrap_or(0);
    format!("{}_{}", ADJ[(n & 15) as usize], NOUN[(n >> 4 & 15) as usize])
}

/// Signal number from "SIGTERM", "TERM", "15".
pub fn signal_number(s: &str) -> Option<i32> {
    if let Ok(n) = s.parse::<i32>() {
        return (1..=64).contains(&n).then_some(n);
    }
    let name = s.trim_start_matches("SIG").to_ascii_uppercase();
    Some(match name.as_str() {
        "HUP" => 1,
        "INT" => 2,
        "QUIT" => 3,
        "ILL" => 4,
        "TRAP" => 5,
        "ABRT" | "IOT" => 6,
        "BUS" => 7,
        "FPE" => 8,
        "KILL" => 9,
        "USR1" => 10,
        "SEGV" => 11,
        "USR2" => 12,
        "PIPE" => 13,
        "ALRM" => 14,
        "TERM" => 15,
        "STKFLT" => 16,
        "CHLD" => 17,
        "CONT" => 18,
        "STOP" => 19,
        "TSTP" => 20,
        "TTIN" => 21,
        "TTOU" => 22,
        "URG" => 23,
        "XCPU" => 24,
        "XFSZ" => 25,
        "VTALRM" => 26,
        "PROF" => 27,
        "WINCH" => 28,
        "IO" | "POLL" => 29,
        "PWR" => 30,
        "SYS" => 31,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_and_parses_rfc3339() {
        assert_eq!(rfc3339(0), "0001-01-01T00:00:00Z");
        let ns = 1_793_448_000_123_456_789; // 2026-10-31T12:00:00.123456789Z
        assert_eq!(rfc3339(ns), "2026-10-31T12:00:00.123456789Z");
        assert_eq!(parse_rfc3339("2026-10-31T12:00:00.123456789Z"), Some(ns));
        assert_eq!(parse_rfc3339("2026-10-31T14:00:00+02:00"), Some(1_793_448_000_000_000_000));
        assert_eq!(parse_rfc3339("2024-02-29T00:00:00Z"), Some(1_709_164_800_000_000_000));
        assert_eq!(parse_rfc3339("garbage"), None);
    }

    #[test]
    fn base64_round_trip() {
        for s in ["", "f", "fo", "foo", "foob", "fooba", "foobar"] {
            assert_eq!(base64_decode(&base64_encode(s.as_bytes())).unwrap(), s.as_bytes());
        }
        assert_eq!(base64_encode(b"user:pass"), "dXNlcjpwYXNz");
        // URL-safe without padding, as sent in X-Registry-Auth.
        assert_eq!(base64_decode("eyJ1c2VybmFtZSI6ImEifQ").unwrap(), br#"{"username":"a"}"#);
    }

    #[test]
    fn signals_and_durations() {
        assert_eq!(signal_number("SIGKILL"), Some(9));
        assert_eq!(signal_number("term"), Some(15));
        assert_eq!(signal_number("2"), Some(2));
        assert_eq!(signal_number("NOPE"), None);
        assert_eq!(human_duration(300), "5 minutes");
        assert_eq!(human_duration(90), "About a minute");
        assert_eq!(human_duration(3 * 86_400), "3 days");
    }

    #[test]
    fn names_are_deterministic_per_id() {
        assert_eq!(random_name("00000000ffff"), "brave_babbage");
        assert_eq!(random_id().len(), 64);
    }
}
