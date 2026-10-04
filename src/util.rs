//! Small shared helpers: errors, formatting, URL quoting, lexical path handling.

use std::fmt;
use std::io::IsTerminal;
use std::time::{SystemTime, UNIX_EPOCH};

/// An expected, user-facing failure: printed as `error: <msg>`, exit code 1.
#[derive(Debug)]
pub struct Fatal(pub String);

impl fmt::Display for Fatal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl From<std::io::Error> for Fatal {
    fn from(e: std::io::Error) -> Self {
        Fatal(e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Fatal>;

pub fn fatal<T>(msg: impl Into<String>) -> Result<T> {
    Err(Fatal(msg.into()))
}

pub fn stderr_is_tty() -> bool {
    std::io::stderr().is_terminal()
}

/// Make text safe to print on a terminal: control characters (C0 except newline and tab, DEL, and C1,
/// which covers ESC and CSI) become `\u{..}` escapes. Text from the server (pipeline, artifact and file
/// names, build error messages, HTTP error bodies) passes through this before it is printed, so it cannot
/// inject escape sequences that recolour, move the cursor or hide output.
pub fn clean(s: &str) -> String {
    if !s.chars().any(is_unsafe) {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 8);
    for c in s.chars() {
        if is_unsafe(c) {
            out.push_str(&format!("\\u{{{:x}}}", c as u32));
        } else {
            out.push(c);
        }
    }
    out
}

fn is_unsafe(c: char) -> bool {
    c.is_control() && c != '\n' && c != '\t'
}

/// Print a warning (also under `-q`), above the progress display if one is visible.
pub fn warn(msg: &str) {
    crate::progress::print_above(&format!("warning: {msg}"));
}

/// Render an error with its source chain, without the (possibly secret) request URL.
pub fn net_err(e: reqwest::Error) -> String {
    let e = e.without_url();
    let mut s = e.to_string();
    let mut src = std::error::Error::source(&e);
    while let Some(c) = src {
        let t = c.to_string();
        if !s.contains(&t) {
            s.push_str(": ");
            s.push_str(&t);
        }
        src = c.source();
    }
    s
}

/// Insert thousands separators into a plain digit string.
pub fn thousands(digits: &str) -> String {
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// Decimal megabytes, rounded, with thousands separators: `1,234 MB`.
pub fn mb(n: u64) -> String {
    format!("{} MB", mb_num(n as f64))
}

pub fn mb_num(n: f64) -> String {
    thousands(&format!("{:.0}", n / 1e6))
}

pub fn hms(seconds: Option<f64>) -> String {
    match seconds {
        Some(s) if s.is_finite() => {
            let s = s.max(0.0) as u64;
            let (h, rem) = (s / 3600, s % 3600);
            let (m, s) = (rem / 60, rem % 60);
            if h > 0 { format!("{h}:{m:02}:{s:02}") } else { format!("{m:02}:{s:02}") }
        }
        _ => "--:--".to_string(),
    }
}

/// Local wall-clock time `HH:MM:SS` at `secs_from_now` seconds in the future.
pub fn clock_after(secs_from_now: f64) -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs_f64()).unwrap_or(0.0);
    let t = (now + secs_from_now) as libc::time_t;
    // SAFETY: localtime_r only writes into the zeroed tm we hand it.
    let tm = unsafe {
        let mut tm: libc::tm = std::mem::zeroed();
        libc::localtime_r(&t, &mut tm);
        tm
    };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

/// Width of the controlling terminal on stderr, if it can be determined.
pub fn term_width() -> usize {
    // SAFETY: TIOCGWINSZ fills a winsize struct; failure leaves it zeroed.
    let w = unsafe {
        let mut ws: libc::winsize = std::mem::zeroed();
        if libc::ioctl(2, libc::TIOCGWINSZ, &mut ws) == 0 { ws.ws_col as usize } else { 0 }
    };
    if w > 0 { w } else { 120 }
}

/// Raise the soft open-file limit; many small output files may be in flight at once.
pub fn raise_nofile_limit() {
    // SAFETY: plain getrlimit/setrlimit calls on a local struct.
    unsafe {
        let mut rl: libc::rlimit = std::mem::zeroed();
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut rl) == 0 {
            let mut want = rl.rlim_max;
            if cfg!(target_os = "macos") {
                want = want.min(10240); // OPEN_MAX; setting more is rejected
            }
            if want > rl.rlim_cur {
                rl.rlim_cur = want;
                libc::setrlimit(libc::RLIMIT_NOFILE, &rl);
            }
        }
    }
}

// --- URL quoting (Python urllib.parse semantics) -------------------------------------------

fn quote_with(s: &str, safe: &str, plus: bool) -> String {
    let mut out = String::with_capacity(s.len());
    for &b in s.as_bytes() {
        let c = b as char;
        if c.is_ascii_alphanumeric() || "_.-~".contains(c) || safe.contains(c) {
            out.push(c);
        } else if plus && c == ' ' {
            out.push('+');
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// `urllib.parse.quote(s)` (keeps `/`).
pub fn quote(s: &str) -> String {
    quote_with(s, "/", false)
}

/// `urllib.parse.urlencode(pairs)`.
pub fn urlencode(pairs: &[(&str, String)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", quote_with(k, "", true), quote_with(v, "", true)))
        .collect::<Vec<_>>()
        .join("&")
}

/// `urllib.parse.unquote(s)`.
pub fn unquote(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let (Some(h), Some(l)) = ((b[i + 1] as char).to_digit(16), (b[i + 2] as char).to_digit(16))
        {
            out.push((h * 16 + l) as u8);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

// --- lexical paths (posixpath.normpath / abspath / relpath) --------------------------------

pub fn normpath(path: &str) -> String {
    if path.is_empty() {
        return ".".into();
    }
    let abs = path.starts_with('/');
    let mut parts: Vec<&str> = Vec::new();
    for c in path.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".." {
            if parts.last().is_some_and(|l| *l != "..") {
                parts.pop();
            } else if !abs {
                parts.push("..");
            }
        } else {
            parts.push(c);
        }
    }
    let joined = parts.join("/");
    match (abs, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

pub fn abspath(path: &str) -> String {
    if path.starts_with('/') {
        normpath(path)
    } else {
        let cwd = std::env::current_dir().map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        normpath(&format!("{cwd}/{path}"))
    }
}

/// `os.path.relpath(path, start)` for two paths relative to the same (unknown) base.
pub fn relpath(path: &str, start: &str) -> String {
    let p = normpath(path);
    let s = normpath(start);
    let pc: Vec<&str> = p.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let sc: Vec<&str> = s.split('/').filter(|c| !c.is_empty() && *c != ".").collect();
    let common = pc.iter().zip(sc.iter()).take_while(|(a, b)| a == b).count();
    let mut out: Vec<&str> = vec![".."; sc.len() - common];
    out.extend(&pc[common..]);
    if out.is_empty() { ".".into() } else { out.join("/") }
}

/// Python `repr()` of a plain string, good enough for messages.
pub fn repr(s: &str) -> String {
    if s.contains('\'') && !s.contains('"') { format!("\"{s}\"") } else { format!("'{}'", s.replace('\'', "\\'")) }
}

pub fn expand_user(p: &str) -> String {
    if (p == "~" || p.starts_with("~/"))
        && let Ok(home) = std::env::var("HOME")
    {
        return format!("{home}{}", &p[1..]);
    }
    p.to_string()
}

#[cfg(test)]
mod tests {
    use super::clean;

    #[test]
    fn clean_neutralises_terminal_control_characters() {
        assert_eq!(clean("plain › text ✓"), "plain › text ✓");
        assert_eq!(clean("line one\nline\ttwo"), "line one\nline\ttwo");
        assert_eq!(clean("\x1b[2J\x1b[31mred"), "\\u{1b}[2J\\u{1b}[31mred");
        assert_eq!(clean("a\rb\x07c\x7f"), "a\\u{d}b\\u{7}c\\u{7f}");
        assert_eq!(clean("\u{9b}31m"), "\\u{9b}31m"); // C1 CSI
        assert!(!clean("\x1b]0;title\x07").contains('\x1b'));
    }
}
