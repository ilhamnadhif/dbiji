//! Headless typing-latency benchmark.
//!
//! ```sh
//! cargo run --release -p dbiji-editor --example typing_bench
//! ```
//!
//! For 1k / 10k / 50k-line SQL documents it drives `SqlEditor` through real
//! `egui::Context` frames (`run_ui` + `tessellate`, i.e. all CPU work of a
//! frame except the GPU upload) and prints per-frame CPU times: opening the
//! document, idle frames, typing one character per frame in the middle of
//! the document (letters, spaces, brackets, newlines), the frame after a
//! typing pause (debounced full re-highlight), paging and select-all.
//! Every configuration runs in its own process so the RSS figures are
//! not polluted by the previous run.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use dbiji_editor::{
    BasicSqlLanguage, EditorLanguage, EditorOptions, EditorState, EditorTheme, SqlEditor, TableInfo,
};
use egui::{Event, Key, Modifiers, Pos2, RawInput, Rect, vec2};

/// Realistic SQL script: ~38 bytes per line on average.
fn make_doc(lines: usize) -> String {
    let mut s = String::with_capacity(lines * 40);
    for i in 0..lines {
        match i % 10 {
            0 => s.push_str(&format!(
                "SELECT u.id, u.name, count(o.id) -- {}\n",
                i % 1000
            )),
            1 => s.push_str("  FROM users u JOIN orders o ON o.user_id = u.id\n"),
            2 => s.push_str("  WHERE u.created_at >= '2026-01-01'\n"),
            3 => s.push_str(&format!(
                "  GROUP BY u.id HAVING sum(o.total) > {};\n",
                i % 500
            )),
            4 => s.push_str("/* block comment describing the next\n"),
            5 => s.push_str("   statement, spanning two lines */\n"),
            6 => s.push_str(&format!(
                "INSERT INTO t (a, b) VALUES ({}, 'n''x');\n",
                i % 100
            )),
            7 => s.push_str("UPDATE p SET price = price * 1.05;\n"),
            8 => s.push_str("DELETE FROM q WHERE id = :id;\n"),
            _ => s.push('\n'),
        }
    }
    s
}

/// Counts live heap bytes (and the peak) to report exact memory use.
struct CountingAlloc;

static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

// SAFETY: delegates every call to the system allocator unchanged and only
// updates two atomic counters.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: same contract as `System.alloc`.
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            let live = LIVE.fetch_add(layout.size(), Ordering::Relaxed) + layout.size();
            PEAK.fetch_max(live, Ordering::Relaxed);
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as `System.dealloc`.
        unsafe { System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: same contract as `System.realloc`.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                let live = LIVE.fetch_add(new_size - layout.size(), Ordering::Relaxed) + new_size
                    - layout.size();
                PEAK.fetch_max(live, Ordering::Relaxed);
            } else {
                LIVE.fetch_sub(layout.size() - new_size, Ordering::Relaxed);
            }
        }
        p
    }
}

#[global_allocator]
static GLOBAL: CountingAlloc = CountingAlloc;

fn heap_mb() -> f64 {
    LIVE.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0)
}

fn rss_mb() -> Option<f64> {
    let out = std::process::Command::new("ps")
        .args(["-o", "rss=", "-p", &std::process::id().to_string()])
        .output()
        .ok()?;
    let kb: f64 = String::from_utf8_lossy(&out.stdout).trim().parse().ok()?;
    Some(kb / 1024.0)
}

struct Bench {
    ctx: egui::Context,
    state: EditorState,
    lang: BasicSqlLanguage,
    options: EditorOptions,
    theme: EditorTheme,
    time: f64,
}

impl Bench {
    fn frame(&mut self, events: Vec<Event>) -> Duration {
        self.time += 1.0 / 60.0;
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, vec2(1280.0, 800.0))),
            time: Some(self.time),
            focused: true,
            events,
            ..Default::default()
        };
        let t0 = Instant::now();
        let (state, lang, options, theme) =
            (&mut self.state, &self.lang, &self.options, &self.theme);
        let out = self.ctx.run_ui(input, |ui| {
            let _ = SqlEditor::new(state, lang, options, theme).show(ui);
        });
        let egui::FullOutput {
            shapes,
            pixels_per_point,
            mut textures_delta,
            ..
        } = out;
        let prims = self.ctx.tessellate(shapes, pixels_per_point);
        std::hint::black_box(&prims);
        let dt = t0.elapsed();
        textures_delta.clear();
        dt
    }
}

struct Stats(Vec<f64>);

impl Stats {
    fn new() -> Self {
        Self(Vec::new())
    }
    fn push(&mut self, d: Duration) {
        self.0.push(d.as_secs_f64() * 1000.0);
    }
    fn fmt(&self) -> String {
        let mut v = self.0.clone();
        v.sort_by(f64::total_cmp);
        let avg = v.iter().sum::<f64>() / v.len().max(1) as f64;
        let p95 = v
            .get((v.len() as f64 * 0.95) as usize)
            .copied()
            .unwrap_or(0.0);
        let max = v.last().copied().unwrap_or(0.0);
        format!("avg {avg:6.2}  p95 {p95:6.2}  max {max:6.2} ms")
    }
}

fn press(key: Key, modifiers: Modifiers) -> Vec<Event> {
    vec![
        Event::Key {
            key,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers,
        },
        Event::Key {
            key,
            physical_key: None,
            pressed: false,
            repeat: false,
            modifiers,
        },
    ]
}

fn run(lines: usize, word_wrap: bool) {
    let rss_start = rss_mb();
    let heap_start = heap_mb();
    let text = make_doc(lines);
    let bytes = text.len();
    let lang = BasicSqlLanguage::new().with_tables(vec![TableInfo {
        name: "users".into(),
        columns: vec!["id".into(), "name".into()],
    }]);
    let hl_start = Instant::now();
    let spans = lang.highlight(&text).len();
    let hl_ms = hl_start.elapsed().as_secs_f64() * 1000.0;
    let mut b = Bench {
        ctx: egui::Context::default(),
        state: EditorState::new(("bench", lines), text),
        lang,
        options: EditorOptions {
            cursor_blink: false,
            word_wrap,
            ..EditorOptions::default()
        },
        theme: EditorTheme::dark(),
        time: 0.0,
    };
    // Warm up egui itself (font atlas, styles) so "open" measures the editor.
    {
        let mut warm = EditorState::new("warm", "SELECT 1;".to_owned());
        let (lang, options, theme) = (&b.lang, &b.options, &b.theme);
        let out = b.ctx.run_ui(RawInput::default(), |ui| {
            let _ = SqlEditor::new(&mut warm, lang, options, theme).show(ui);
        });
        out.drop_without_applying_deltas();
    }
    let rss_loaded = rss_mb();
    let heap_loaded = heap_mb();
    PEAK.store(LIVE.load(Ordering::Relaxed), Ordering::Relaxed);
    let open = b.frame(vec![]);
    let heap_open = heap_mb();
    let ctx = b.ctx.clone();
    b.state.request_focus(&ctx);
    b.frame(vec![]);
    let mid = b.state.text().len() / 2;
    let mid = b.state.line_range(b.state.line_of_offset(mid)).end;
    b.state.set_cursor(&ctx, mid);
    for _ in 0..5 {
        b.frame(vec![]);
    }
    // Let word-wrap refinement finish before measuring idle frames.
    let mut settle = Stats::new();
    for _ in 0..400 {
        settle.push(b.frame(vec![]));
    }

    let mut idle = Stats::new();
    for _ in 0..60 {
        idle.push(b.frame(vec![]));
    }

    let len_before = b.state.text().len();
    let mut typing = Stats::new();
    let script = " AND col_name = func(arg) OR x LIKE 'abc'";
    for i in 0..400 {
        let events = if i % 60 == 59 {
            press(Key::Enter, Modifiers::NONE)
        } else {
            let c = script.chars().nth(i % script.len()).unwrap_or('x');
            vec![Event::Text(c.to_string())]
        };
        typing.push(b.frame(events));
    }
    let typed = b.state.text().len() - len_before;
    assert!(
        typed >= 390,
        "typing did not reach the editor ({typed} bytes)"
    );
    // The first frame after a pause runs the debounced full re-highlight.
    b.time += 0.5;
    let pause = b.frame(vec![]);

    let mut paging = Stats::new();
    for _ in 0..40 {
        paging.push(b.frame(press(Key::PageDown, Modifiers::NONE)));
    }
    let cmd = if cfg!(target_os = "macos") {
        Modifiers::MAC_CMD.plus(Modifiers::COMMAND)
    } else {
        Modifiers::CTRL.plus(Modifiers::COMMAND)
    };
    b.frame(press(Key::A, cmd));
    let mut selected = Stats::new();
    for _ in 0..20 {
        selected.push(b.frame(vec![]));
    }
    let rss_end = rss_mb();
    let heap_end = heap_mb();
    let heap_peak = PEAK.load(Ordering::Relaxed) as f64 / (1024.0 * 1024.0);
    let text_mb = b.state.text().len() as f64 / (1024.0 * 1024.0);

    println!(
        "{lines:>6} lines {:>7.1} KB{}  open {:6.1} ms  (highlighter alone {hl_ms:.1} ms, {spans} tokens)",
        bytes as f64 / 1024.0,
        if word_wrap { " wrap" } else { "     " },
        open.as_secs_f64() * 1000.0
    );
    if word_wrap {
        println!("    wrap refinement  {}", settle.fmt());
    }
    println!("    idle frame       {}", idle.fmt());
    println!(
        "    typing (middle)  {}   ({typed} bytes typed)",
        typing.fmt()
    );
    println!(
        "    pause (full hl)  {:6.2} ms",
        pause.as_secs_f64() * 1000.0
    );
    println!("    page down        {}", paging.fmt());
    println!("    select-all idle  {}", selected.fmt());
    println!(
        "    heap: egui+text loaded {:.1} MB (text {text_mb:.1} MB), after open {:.1} MB, after run {:.1} MB, peak {:.1} MB (baseline {:.1})",
        heap_loaded, heap_open, heap_end, heap_peak, heap_start
    );
    if let (Some(a), Some(l), Some(z)) = (rss_start, rss_loaded, rss_end) {
        println!(
            "    RSS: process start {a:.1} MB, document loaded {l:.1} MB, after run {z:.1} MB"
        );
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let (Some(lines), Some(wrap)) = (args.get(1), args.get(2)) {
        let lines = lines.parse().unwrap_or(1000);
        run(lines, wrap == "wrap");
        return;
    }
    println!("DBiji editor typing benchmark (CPU time per frame: egui run_ui + tessellate)");
    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot locate benchmark executable: {e}");
            return;
        }
    };
    for (lines, wrap) in [
        (1_000, "nowrap"),
        (10_000, "nowrap"),
        (50_000, "nowrap"),
        (10_000, "wrap"),
    ] {
        match std::process::Command::new(&exe)
            .arg(lines.to_string())
            .arg(wrap)
            .status()
        {
            Ok(s) if s.success() => {}
            Ok(s) => eprintln!("run {lines} {wrap} failed: {s}"),
            Err(e) => eprintln!("run {lines} {wrap} failed: {e}"),
        }
    }
}
