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
pub mod drivers;
pub mod exec;
pub mod headless;
pub mod keys;
pub mod nav;
pub mod ops;
pub mod palette;
pub mod results;
pub mod router;
pub mod services;
pub mod sqllang;
pub mod sqlprep;
pub mod taskexec;
pub mod tools;
pub mod transferexec;
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

/// All drivers built into this binary.
pub fn driver_registry() -> dbiji_core::registry::DriverRegistry {
    let mut reg = dbiji_core::registry::DriverRegistry::new();
    drivers::register_all(&mut reg);
    reg
}

/// Connection manager over the saved connections, the configured secret
/// store, SSH tunnels / proxies and `~/.pgpass`.
pub fn connection_manager(services: &services::Services) -> (conn::ConnectionManager, Vec<String>) {
    let (store, warnings) = dbiji_config::ConnectionStore::load_or_recover(&services.paths);
    let secrets = Arc::new(conn::ManagedSecrets(services.secrets.clone()));
    let net = Arc::new(conn::TunnelNet { known_hosts: services.paths.known_hosts_file() });
    let mut conns = conn::ConnectionManager::new(driver_registry(), secrets, net);
    conns.store = store;
    conns.pgpass = Some(Arc::new(|host: &str, port: u16, db: &str, user: &str| dbiji_config::import::pgpass::lookup(host, port, db, user)));
    (conns, warnings)
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
    let paths = match dbiji_config::AppPaths::new() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("cannot determine the configuration directory: {e}");
            return ExitCode::FAILURE;
        }
    };
    let (settings, mut warnings) = dbiji_config::settings::load_with_warnings(&paths);
    dbiji_core::i18n::set_locale(settings.general.effective_locale());
    settings.results.apply_memory_limits();

    let backend = match backend::Backend::new() {
        Ok(b) => Arc::new(b),
        Err(e) => {
            eprintln!("cannot start the I/O runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Some(task) = &args.run_task {
        return headless::run_task(&backend, &paths, &settings, task);
    }
    let mut services = services::Services::load(paths, &settings);
    services.warnings.append(&mut warnings);
    let (conns, mut conn_warnings) = connection_manager(&services);
    services.warnings.append(&mut conn_warnings);

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
    let files = args.files.clone();
    let result = eframe::run_native(
        "DBiji",
        options,
        Box::new(move |cc| {
            theme::setup_fonts(&cc.egui_ctx);
            theme::apply_settings(&cc.egui_ctx, &settings);
            backend.set_context(cc.egui_ctx.clone());
            let mut app = app::DbijiApp::new(backend, conns, services, settings);
            app.exit_after_first_frame = exit_after_first;
            if app.settings.general.restore_session && std::env::var_os("DBIJI_BENCH_SCENARIO").is_none() {
                app.restore_session();
            }
            for f in &files {
                app.open_path(std::path::Path::new(f));
            }
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
