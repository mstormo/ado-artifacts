//! Terminal output: the live progress display, the mapping spinner, and `-q`/`-v` handling.
//!
//! On a TTY two lines at the bottom of stderr (bar line, details line) are redrawn in place,
//! exactly every 200 ms (5 Hz). Everything else that is printed while they are visible (warnings, notices)
//! goes through [`print_above`], which erases the lines, prints the message and redraws them
//! below it. Without a TTY no escape codes are used: the two lines as plain text every 10 s instead.

use std::fmt::Write as _;
use std::io::Write as _;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, AtomicUsize, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;

use crate::util::{clean, clock_after, hms, stderr_is_tty, term_width, thousands};

/// Transient-error retries (blob and container-file fetches), shown in the details line.
pub static RETRIES: AtomicU64 = AtomicU64::new(0);
/// Running totals shown by the mapping spinner.
pub static MAP_NODES: AtomicU64 = AtomicU64::new(0);
pub static MAP_CHUNKS: AtomicU64 = AtomicU64::new(0);

static QUIET: AtomicBool = AtomicBool::new(false);
static VERBOSE: AtomicBool = AtomicBool::new(false);

/// Redraw period of the live display (5 Hz).
pub const FRAME: Duration = Duration::from_millis(200);
/// Interval between plain status lines when stderr is not a terminal.
const PLAIN_EVERY: Duration = Duration::from_secs(10);
/// Time constant of the throughput moving average.
const EMA_TAU: f64 = 2.5;
const MIN_BAR: usize = 10;
/// Width of the percentage field plus the space after it: the column where the bar starts.
const INDENT: usize = 7;
/// Separator between the fields of the details line.
const SEP: &str = " · ";

pub fn init(quiet: bool, verbose: bool) {
    QUIET.store(quiet, Relaxed);
    VERBOSE.store(verbose, Relaxed);
}

pub fn quiet() -> bool {
    QUIET.load(Relaxed)
}

fn use_color() -> bool {
    stderr_is_tty() && std::env::var_os("NO_COLOR").is_none_or(|v| v.is_empty())
}

/// An informational line: suppressed by `-q`.
pub fn info(msg: &str) {
    if !quiet() {
        print_above(msg);
    }
}

/// Finalise the live display (leaving its last frame visible) before an interrupt message.
/// Returns whether a display was active, i.e. whether the cursor is already at column 0.
pub fn interrupted() -> bool {
    screen().end(true)
}

// --- the screen -----------------------------------------------------------------------------

struct Screen {
    enabled: bool,
    hidden: bool,
    lines: usize,
    frame: Vec<String>,
}

static SCREEN: Mutex<Screen> = Mutex::new(Screen { enabled: false, hidden: false, lines: 0, frame: Vec::new() });

fn screen() -> MutexGuard<'static, Screen> {
    SCREEN.lock().unwrap_or_else(|e| e.into_inner())
}

/// One write per update, so a frame never shows up half drawn.
fn emit(s: &str) {
    let mut e = std::io::stderr().lock();
    let _ = e.write_all(s.as_bytes());
    let _ = e.flush();
}

fn paint(frame: &[String], out: &mut String) {
    for (i, l) in frame.iter().enumerate() {
        if i > 0 {
            out.push('\n');
        }
        out.push_str("\x1b[2K");
        out.push_str(l);
    }
}

impl Screen {
    fn begin(&mut self) {
        if self.enabled || quiet() || !stderr_is_tty() {
            return;
        }
        self.enabled = true;
        self.hidden = true;
        self.lines = 0;
        self.frame.clear();
        emit("\x1b[?25l");
    }

    /// Move the cursor to the start of the first drawn line.
    fn rewind(&self, out: &mut String) {
        if self.lines > 1 {
            let _ = write!(out, "\x1b[{}A", self.lines - 1);
        }
        out.push('\r');
    }

    fn draw(&mut self, frame: Vec<String>) {
        if !self.enabled || frame.is_empty() {
            return;
        }
        let mut out = String::new();
        self.rewind(&mut out);
        paint(&frame, &mut out);
        out.push_str("\x1b[J");
        emit(&out);
        self.lines = frame.len();
        self.frame = frame;
    }

    /// Stop drawing. `keep` leaves the last frame on screen (and moves below it); otherwise it is erased.
    fn end(&mut self, keep: bool) -> bool {
        if !self.enabled {
            return false;
        }
        let mut out = String::new();
        if self.lines > 0 {
            self.rewind(&mut out);
            if keep {
                paint(&self.frame, &mut out);
                out.push_str("\x1b[J\n");
            } else {
                out.push_str("\x1b[J");
            }
        }
        if self.hidden {
            out.push_str("\x1b[?25h");
        }
        emit(&out);
        self.enabled = false;
        self.hidden = false;
        self.lines = 0;
        self.frame.clear();
        true
    }
}

/// Print a line of text above the live display (or just print it when there is none).
pub fn print_above(msg: &str) {
    let msg = &clean(msg);
    let s = screen();
    let mut out = String::new();
    if s.enabled && s.lines > 0 {
        s.rewind(&mut out);
        out.push_str("\x1b[J");
        out.push_str(msg);
        out.push('\n');
        paint(&s.frame, &mut out);
    } else {
        out.push_str(msg);
        out.push('\n');
    }
    emit(&out);
}

// --- mapping spinner --------------------------------------------------------------------------

/// Spinner line shown while the node trees are mapped; erased when dropped.
pub struct MapSpinner {
    task: Option<JoinHandle<()>>,
}

impl MapSpinner {
    pub fn start() -> Self {
        MAP_NODES.store(0, Relaxed);
        MAP_CHUNKS.store(0, Relaxed);
        let on = {
            let mut s = screen();
            s.begin();
            s.enabled
        };
        let task = on.then(|| {
            tokio::spawn(async move {
                const FRAMES: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];
                let color = use_color();
                let t0 = Instant::now();
                let mut iv = tokio::time::interval(FRAME);
                iv.set_missed_tick_behavior(MissedTickBehavior::Skip);
                let mut n = 0usize;
                loop {
                    iv.tick().await;
                    let spin = if color { format!("\x1b[36m{}\x1b[0m", FRAMES[n % FRAMES.len()]) } else { FRAMES[n % FRAMES.len()].to_string() };
                    let line = format!(
                        "{spin} Mapping files  {} nodes, {} chunks  {}",
                        thousands(&MAP_NODES.load(Relaxed).to_string()),
                        thousands(&MAP_CHUNKS.load(Relaxed).to_string()),
                        hms(Some(t0.elapsed().as_secs_f64()))
                    );
                    screen().draw(vec![line]);
                    n += 1;
                }
            })
        });
        MapSpinner { task }
    }
}

impl Drop for MapSpinner {
    fn drop(&mut self) {
        if let Some(t) = self.task.take() {
            t.abort();
        }
        screen().end(false);
    }
}

// --- download progress ------------------------------------------------------------------------

struct Render {
    last_t: Instant,
    last_done: i64,
    /// Debiased exponential moving average of the byte rate: `num / den`.
    num: f64,
    den: f64,
    last_plain: Option<Instant>,
}

pub struct Progress {
    total: u64,
    files_total: u64,
    blocks_total: u64,
    done: AtomicI64,
    files_done: AtomicU64,
    blocks_done: AtomicU64,
    base: i64,
    tty: bool,
    color: bool,
    verbose: bool,
    start: Instant,
    /// Widest time text seen so far (`MM:SS` or `H:MM:SS`), so the columns do not jitter.
    time_w: AtomicUsize,
    render: Mutex<Render>,
}

struct Snap {
    done: u64,
    frac: f64,
    files_done: u64,
    blocks_done: u64,
    speed: f64,
    left: Option<f64>,
    elapsed: f64,
    retries: u64,
}

const GLYPHS: [&str; 8] = [" ", "▏", "▎", "▍", "▌", "▋", "▊", "▉"];

fn bar(frac: f64, width: usize, color: bool) -> String {
    let eighths = ((frac.clamp(0.0, 1.0) * width as f64 * 8.0) as usize).min(width * 8);
    let (full, part) = (eighths / 8, eighths % 8);
    let rest = width - full - usize::from(part > 0);
    let mut s = String::new();
    if color {
        s.push_str("\x1b[32m");
    }
    s.push_str(&"█".repeat(full));
    if part > 0 {
        s.push_str(GLYPHS[part]);
    }
    if color {
        s.push_str("\x1b[0m\x1b[90m");
    }
    s.push_str(&"░".repeat(rest));
    if color {
        s.push_str("\x1b[0m");
    }
    s
}

/// Decimal megabytes with thousands separators; one decimal for small totals.
fn mb_str(n: f64, dp: usize) -> String {
    let s = format!("{:.dp$}", n / 1e6);
    match s.split_once('.') {
        Some((i, f)) => format!("{}.{f}", thousands(i)),
        None => thousands(&s),
    }
}

impl Progress {
    /// `done` is the byte count already present from a resumed download; `blocks_total` the
    /// number of chunks (and container files) this run fetches.
    pub fn new(total: u64, files_total: u64, done: u64, blocks_total: u64) -> Self {
        let now = Instant::now();
        let tty = stderr_is_tty() && !quiet();
        if tty {
            screen().begin();
        }
        Progress {
            total,
            files_total,
            blocks_total,
            done: AtomicI64::new(done as i64),
            files_done: AtomicU64::new(0),
            blocks_done: AtomicU64::new(0),
            base: done as i64,
            tty,
            color: tty && use_color(),
            verbose: VERBOSE.load(Relaxed),
            start: now,
            time_w: AtomicUsize::new(5),
            render: Mutex::new(Render { last_t: now, last_done: done as i64, num: 0.0, den: 0.0, last_plain: None }),
        }
    }

    pub fn add(&self, bytes: u64, file_done: bool) {
        self.done.fetch_add(bytes as i64, Relaxed);
        if file_done {
            self.files_done.fetch_add(1, Relaxed);
        }
    }

    pub fn sub(&self, bytes: u64) {
        self.done.fetch_sub(bytes as i64, Relaxed);
    }

    /// One chunk (or container file) is downloaded, verified and written everywhere.
    pub fn block_done(&self) {
        self.blocks_done.fetch_add(1, Relaxed);
    }

    fn snapshot(&self, st: &Render, final_: bool) -> Snap {
        let done = self.done.load(Relaxed).max(0) as u64;
        let elapsed = self.start.elapsed().as_secs_f64();
        let avg = if elapsed > 0.0 { done.saturating_sub(self.base as u64) as f64 / elapsed } else { 0.0 };
        let speed = if final_ {
            avg
        } else if st.den > 0.0 {
            st.num / st.den
        } else {
            0.0
        };
        let left = if done >= self.total {
            Some(0.0)
        } else if !final_ && speed > 1.0 {
            Some((self.total - done) as f64 / speed)
        } else {
            None
        };
        Snap {
            done,
            frac: if self.total > 0 { (done as f64 / self.total as f64).min(1.0) } else { 1.0 },
            files_done: self.files_done.load(Relaxed),
            blocks_done: self.blocks_done.load(Relaxed),
            speed,
            left,
            elapsed,
            retries: RETRIES.load(Relaxed),
        }
    }

    /// A time as `MM:SS` (or `H:MM:SS`), right-aligned to the widest one seen so far.
    fn time(&self, v: Option<f64>) -> String {
        let t = hms(v);
        let w = self.time_w.fetch_max(t.len(), Relaxed).max(t.len());
        format!("{t:>w$}")
    }

    fn mb_dp(&self) -> usize {
        if self.total < 100_000_000 { 1 } else { 0 }
    }

    fn pct(s: &Snap) -> String {
        format!("{:5.1}%", ((s.frac * 1000.0).floor() / 10.0).min(100.0))
    }

    /// Remaining MB and remaining time, fixed width.
    fn left_parts(&self, s: &Snap) -> (String, String) {
        let total_w = mb_str(self.total as f64, self.mb_dp()).len();
        (format!("{:>total_w$} MB", mb_str(self.total.saturating_sub(s.done) as f64, self.mb_dp())), self.time(s.left))
    }

    /// Line 1 text after the bar (labels omitted); `keep` is how many of the two fields are kept (2, 1 = only the time, 0).
    fn left_fields(&self, s: &Snap, keep: usize) -> String {
        let (mbl, tl) = self.left_parts(s);
        match keep {
            2 => format!(" {mbl} {tl}"),
            1 => format!(" {tl}"),
            _ => String::new(),
        }
    }

    /// Line 1: `pct │bar│ remaining-MB remaining-time`. The bar takes the remaining width, at least [`MIN_BAR`]
    /// cells (the MB field, then the time field, are dropped to make room).
    fn bar_line(&self, s: &Snap, width: usize, bar_color: bool) -> String {
        let pct = Self::pct(s);
        let (tail, bar_w) = (0..=2)
            .rev()
            .map(|keep| {
                let tail = self.left_fields(s, keep);
                let avail = width.saturating_sub(pct.len() + 3 + tail.chars().count());
                (tail, avail)
            })
            .find(|(_, avail)| *avail >= MIN_BAR)
            .unwrap_or_else(|| (String::new(), width.saturating_sub(pct.len() + 3).max(4)));
        let paint = |style: &str, t: &str| if bar_color { format!("\x1b[{style}m{t}\x1b[0m") } else { t.to_string() };
        format!("{} {}{}{}{tail}", paint("1;32", &pct), paint("2", "│"), bar(s.frac, bar_w, bar_color), paint("2", "│"))
    }

    /// The fields of line 2 that are still shown at drop `level` (a field is shown while its rank is
    /// above `level`; throughput goes first). Each is `(text, colour style)`.
    fn detail_fields(&self, s: &Snap, level: u8) -> Vec<(String, &'static str)> {
        let dp = self.mb_dp();
        let total_mb = mb_str(self.total as f64, dp);
        let files_w = self.files_total.to_string().len();
        let blocks_total = thousands(&self.blocks_total.to_string());
        let mut v: Vec<(String, &'static str)> = Vec::new();
        if self.verbose && level < 6 {
            v.push((format!("{:>w$}/{blocks_total} blocks", thousands(&s.blocks_done.to_string()), w = blocks_total.len()), "2"));
        }
        if level < 5 {
            v.push((format!("{:>files_w$}/{} files", s.files_done, self.files_total), ""));
        }
        if level < 7 {
            v.push((format!("{:>w$}/{total_mb} MB", mb_str(s.done as f64, dp), w = total_mb.len()), ""));
        }
        if level < 1 {
            v.push((format!("{:>5.1} MB/s", s.speed / 1e6), "36"));
        }
        // Time left is on line 1; line 2 has elapsed time and the local clock time of completion.
        let mut times = Vec::new();
        if level < 3 {
            times.push(self.time(Some(s.elapsed)));
        }
        if level < 2 {
            times.push(format!("ETA {}", s.left.map_or_else(|| "--:--:--".to_string(), clock_after)));
        }
        if !times.is_empty() {
            v.push((times.join(" / "), ""));
        }
        if s.retries > 0 && level < 4 {
            v.push((format!("retries {}", s.retries), "33"));
        }
        v
    }

    /// Line 2 as `(plain width in cells, fields)`, dropping the lowest-priority fields until it fits `width`.
    fn details(&self, s: &Snap, width: Option<usize>) -> Vec<(String, &'static str)> {
        for level in 0..=9u8 {
            let f = self.detail_fields(s, level);
            let w: usize = f.iter().map(|(t, _)| t.chars().count()).sum::<usize>() + SEP.chars().count() * f.len().saturating_sub(1);
            if width.is_none_or(|width| w <= width) {
                return f;
            }
        }
        Vec::new()
    }

    /// The two lines of the live display.
    fn frame(&self, s: &Snap) -> Vec<String> {
        // Re-read every frame so a resize shows up on the next redraw. Both lines stay one cell short of the
        // right edge, so nothing ever lands in the last column (a pending wrap would break the in-place redraw).
        let width = term_width().saturating_sub(1).max(20);
        let paint = |style: &str, t: &str| if self.color && !style.is_empty() { format!("\x1b[{style}m{t}\x1b[0m") } else { t.to_string() };
        let sep = paint("2", SEP);
        // Line 2 starts under the bar's opening glyph: after the percentage field and its space.
        let line2 = self.details(s, Some(width.saturating_sub(INDENT))).iter().map(|(t, st)| paint(st, t)).collect::<Vec<_>>().join(&sep);
        vec![self.bar_line(s, width, self.color), format!("{}{line2}", " ".repeat(INDENT))]
    }

    /// The same two lines as plain text without bar glyphs or escape codes, for output that is not a terminal.
    fn plain_text(&self, s: &Snap) -> String {
        let (mbl, tl) = self.left_parts(s);
        let line1 = format!("{}{SEP}{} left{SEP}{tl} left", Self::pct(s).trim_start(), mbl.trim_start());
        let line2 = self.details(s, None).iter().map(|(t, _)| t.as_str()).collect::<Vec<_>>().join(SEP);
        format!("{line1}\n{line2}")
    }

    /// Called every [`FRAME`]: update the throughput average and redraw (or print a plain line when due).
    pub fn tick(&self) {
        if quiet() {
            return;
        }
        let now = Instant::now();
        let mut st = self.render.lock().unwrap_or_else(|e| e.into_inner());
        let dt = now.duration_since(st.last_t).as_secs_f64();
        if dt > 0.0 {
            let done = self.done.load(Relaxed).max(0);
            let inst = ((done - st.last_done) as f64 / dt).max(0.0);
            let a = 1.0 - (-dt / EMA_TAU).exp();
            st.num = (1.0 - a) * st.num + a * inst;
            st.den = (1.0 - a) * st.den + a;
            st.last_done = done;
            st.last_t = now;
        }
        let snap = self.snapshot(&st, false);
        if self.tty {
            drop(st);
            screen().draw(self.frame(&snap));
        } else if st.last_plain.is_none_or(|t| now.duration_since(t) >= PLAIN_EVERY) {
            st.last_plain = Some(now);
            drop(st);
            print_above(&self.plain_text(&snap));
        }
    }

    /// Final state (average throughput) stays visible; the cursor moves below the display.
    pub fn finish(&self) {
        if quiet() {
            return;
        }
        let st = self.render.lock().unwrap_or_else(|e| e.into_inner());
        let snap = self.snapshot(&st, true);
        drop(st);
        if self.tty {
            let mut s = screen();
            s.draw(self.frame(&snap));
            s.end(true);
        } else {
            print_above(&self.plain_text(&snap));
        }
    }
}
