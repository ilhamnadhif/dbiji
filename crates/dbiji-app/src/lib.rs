//! DBiji application crate: window, tabs, navigator and the wiring of all
//! DBiji crates. The `dbiji` binary calls [`run`].

pub mod app;
pub mod backend;
pub mod bench;
pub mod bench_ui;
pub mod command;
pub mod conn;
pub mod cx;
pub mod dialogs;
pub mod exec;
pub mod keys;
pub mod nav;
pub mod ops;
pub mod palette;
pub mod results;
pub mod router;
pub mod services;
pub mod tabs;
pub mod theme;
pub mod toasts;
pub mod urls;
pub mod widgets;

use std::process::ExitCode;
use std::sync::Arc;

const HELP: &str = "DBiji - lightweight SQL database client

Usage: dbiji [OPTIONS] [FILE.sql | DATABASE.db ...]

Options:
  --run-task <ID|NAME>     Run a saved task headless and exit
  --bench <NAME>           Run a headless benchmark (startup, memory, grid, editor, all)
  --bench-report <FILE>    Write benchmark results as JSON
  --exit-after-first-frame Exit right after the first frame (startup measurement)
  --portable <DIR>         Keep all settings and data in DIR
  -V, --version            Print version
  -h, --help               Print help";

#[derive(Debug, Default, Clone)]
pub struct CliArgs {
    pub run_task: Option<String>,
    pub bench: Option<String>,
    pub bench_report: Option<String>,
    pub exit_after_first_frame: bool,
    pub portable: Option<String>,
    pub files: Vec<String>,
}

pub fn parse_args(args: impl IntoIterator<Item = String>) -> Result<Option<CliArgs>, String> {
    let mut out = CliArgs::default();
    let mut it = args.into_iter();
    while let Some(a) = it.next() {
        match a.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return Ok(None);
            }
            "-V" | "--version" => {
                println!("dbiji {}", env!("CARGO_PKG_VERSION"));
                return Ok(None);
            }
            "--run-task" => out.run_task = Some(it.next().ok_or("--run-task needs a task id or name")?),
            "--bench" => out.bench = Some(it.next().ok_or("--bench needs a name")?),
            "--bench-report" => out.bench_report = Some(it.next().ok_or("--bench-report needs a file")?),
            "--exit-after-first-frame" => out.exit_after_first_frame = true,
            "--portable" => out.portable = Some(it.next().ok_or("--portable needs a directory")?),
            s if s.starts_with("-psn_") => {} // macOS process serial number
            s if s.starts_with('-') => return Err(format!("unknown option {s}\n\n{HELP}")),
            _ => out.files.push(a),
        }
    }
    Ok(Some(out))
}

fn init_logging() {
    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_env("DBIJI_LOG").unwrap_or_else(|_| EnvFilter::new("warn,dbiji=info"));
    let _ = tracing_subscriber::fmt().with_env_filter(filter).with_target(false).try_init();
}

fn window_icon() -> Option<egui::IconData> {
    let bytes = include_bytes!("../assets/icon-256.png");
    let img = image::load_from_memory(bytes).ok()?.into_rgba8();
    let (w, h) = img.dimensions();
    Some(egui::IconData { rgba: img.into_raw(), width: w, height: h })
}

/// Entry point of the `dbiji` binary.
pub fn run() -> ExitCode {
    bench::mark_process_start();
    let args = match parse_args(std::env::args().skip(1)) {
        Ok(Some(a)) => a,
        Ok(None) => return ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("{e}");
            return ExitCode::from(2);
        }
    };
    init_logging();
    if let Some(name) = &args.bench {
        return bench::run(name, args.bench_report.as_deref());
    }
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();
    if let Some(dir) = &args.portable {
        // SAFETY: called before any other thread is spawned.
        unsafe { std::env::set_var("DBIJI_HOME", dir) };
    }
    dbiji_core::i18n::set_locale(dbiji_core::i18n::system_locale());

    let backend = match backend::Backend::new() {
        Ok(b) => Arc::new(b),
        Err(e) => {
            eprintln!("cannot start the I/O runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    let registry = dbiji_core::registry::DriverRegistry::new();
    let conns = conn::ConnectionManager::new(registry, Arc::new(conn::MemorySecrets::default()), Arc::new(conn::DirectNet));
    let settings = services::Settings::default();

    let mut viewport = egui::ViewportBuilder::default()
        .with_title("DBiji")
        .with_app_id("dev.dbiji.DBiji")
        .with_inner_size([1280.0, 820.0])
        .with_min_inner_size([640.0, 400.0]);
    if let Some(icon) = window_icon() {
        viewport = viewport.with_icon(icon);
    }
    let options = eframe::NativeOptions { viewport, ..Default::default() };
    let exit_after_first = args.exit_after_first_frame;
    let result = eframe::run_native(
        "DBiji",
        options,
        Box::new(move |cc| {
            theme::setup_fonts(&cc.egui_ctx);
            theme::apply_theme(&cc.egui_ctx, settings.theme, settings.high_contrast);
            backend.set_context(cc.egui_ctx.clone());
            let mut app = app::DbijiApp::new(backend, conns, services::Services::default(), settings);
            app.exit_after_first_frame = exit_after_first;
            Ok(Box::new(app))
        }),
    );
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("DBiji failed: {e}");
            ExitCode::FAILURE
        }
    }
}
