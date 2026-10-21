//! "What's new" after an update and the optional check for newer releases.
//!
//! The release notes come from `CHANGELOG.md`, built into the binary. The
//! update check is opt-in (DBiji asks once): at every start and every few
//! hours it reads the latest release of the GitHub repository — one
//! anonymous HTTPS request, nothing about the user or their databases is
//! sent. A newer release is offered with its notes: install it (macOS, see
//! [`crate::selfupdate`]), download it, skip that version or be reminded
//! later; the toolbar shows an Update button meanwhile.

use crate::backend::{Backend, Job};
use crate::services::Settings;
use crate::toasts::Toasts;
use dbiji_core::{tr, trf};
use egui::{Context, RichText};
use egui_phosphor::regular as icon;
use std::time::Duration;

/// The release notes of every version, newest first.
pub const CHANGELOG: &str = include_str!("../../../CHANGELOG.md");

/// Latest release of the repository (GitHub REST API).
const RELEASES_API: &str = "https://api.github.com/repos/ilhamnadhif/dbiji/releases/latest";

/// Where the releases are listed.
pub const RELEASES_PAGE: &str = "https://github.com/ilhamnadhif/dbiji/releases";

/// Version of this build.
pub fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Numeric parts of `1.2.3` / `v1.2.3-beta` (a pre-release counts as older
/// than the release itself).
fn version_key(v: &str) -> (Vec<u64>, bool) {
    let v = v.trim().trim_start_matches(['v', 'V']);
    let (core, pre) = match v.split_once(['-', '+']) {
        Some((c, _)) => (c, true),
        None => (v, false),
    };
    let mut parts: Vec<u64> = core.split('.').map(|p| p.trim().parse().unwrap_or(0)).collect();
    while parts.len() > 1 && parts.last() == Some(&0) {
        parts.pop();
    }
    (parts, !pre)
}

/// `a` is a newer version than `b`.
pub fn is_newer(a: &str, b: &str) -> bool {
    version_key(a) > version_key(b)
}

/// Release notes of one version.
#[derive(Debug, Clone, PartialEq)]
pub struct Notes {
    pub version: String,
    /// Date or other text after the version in the heading.
    pub date: String,
    pub body: String,
}

/// The `## <version> …` sections of a changelog.
pub fn parse_changelog(text: &str) -> Vec<Notes> {
    let mut out: Vec<Notes> = Vec::new();
    for line in text.lines() {
        if let Some(head) = line.strip_prefix("## ") {
            let head = head.trim().trim_start_matches('[');
            let (version, rest) = head.split_once(|c: char| c.is_whitespace() || c == ']').unwrap_or((head, ""));
            let date = rest.trim().trim_start_matches(['-', '—', '–', ']', ' ']).trim().to_string();
            out.push(Notes { version: version.trim_start_matches(['v', 'V']).to_string(), date, body: String::new() });
        } else if let Some(n) = out.last_mut() {
            n.body.push_str(line);
            n.body.push('\n');
        }
    }
    for n in &mut out {
        n.body = n.body.trim().to_string();
    }
    out
}

/// Notes of the versions after `seen`, up to and including `current`, newest first.
pub fn notes_since(changelog: &str, seen: Option<&str>, current: &str) -> Vec<Notes> {
    parse_changelog(changelog).into_iter().filter(|n| !is_newer(&n.version, current) && seen.is_none_or(|s| is_newer(&n.version, s))).collect()
}

/// The app archive of a release, installed by the updater (macOS).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Asset {
    pub name: String,
    pub url: String,
    pub size: u64,
    /// Hex SHA-256, when GitHub published one.
    pub sha256: Option<String>,
}

/// Name of the archive the updater installs.
pub fn archive_name(version: &str) -> String {
    format!("DBiji-{version}.zip")
}

/// A published release.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Release {
    pub version: String,
    /// Release page.
    pub url: String,
    /// The macOS disk image, when the release has one.
    pub download_url: Option<String>,
    pub notes: String,
    /// When it was published (`YYYY-MM-DD`).
    pub date: Option<String>,
    /// `DBiji-<version>.zip`, installed in place by the updater.
    pub archive: Option<Asset>,
}

/// Parse the GitHub "latest release" response.
pub fn parse_release(json: &str) -> Option<Release> {
    let v: serde_json::Value = serde_json::from_str(json).ok()?;
    if v.get("draft").and_then(|d| d.as_bool()) == Some(true) || v.get("prerelease").and_then(|d| d.as_bool()) == Some(true) {
        return None;
    }
    let tag = v.get("tag_name")?.as_str()?;
    let url = v.get("html_url").and_then(|u| u.as_str()).unwrap_or(RELEASES_PAGE).to_string();
    let download_url = v
        .get("assets")
        .and_then(|a| a.as_array())
        .and_then(|assets| assets.iter().filter_map(|a| a.get("browser_download_url")?.as_str()).find(|u| u.ends_with(".dmg")).map(str::to_string));
    let notes = v.get("body").and_then(|b| b.as_str()).unwrap_or("").replace("\r\n", "\n").trim().to_string();
    let version = tag.trim_start_matches(['v', 'V']).to_string();
    let wanted = archive_name(&version);
    let archive = v.get("assets").and_then(|a| a.as_array()).and_then(|assets| {
        let a = assets.iter().find(|a| a.get("name").and_then(|n| n.as_str()) == Some(wanted.as_str()))?;
        Some(Asset {
            name: wanted.clone(),
            url: a.get("browser_download_url")?.as_str()?.to_string(),
            size: a.get("size")?.as_u64()?,
            sha256: a.get("digest").and_then(|d| d.as_str()).and_then(|d| d.strip_prefix("sha256:")).map(str::to_ascii_lowercase),
        })
    });
    let date = v.get("published_at").and_then(|d| d.as_str()).and_then(|d| d.get(..10)).map(str::to_string);
    Some(Release { version, url, download_url, notes, date, archive })
}

/// Outcome of a check.
#[derive(Debug, Clone, PartialEq)]
pub enum CheckResult {
    /// A newer release.
    Available(Release),
    /// This build is the latest (or nothing has been published yet).
    UpToDate,
}

/// Ask GitHub (or `url`) for the latest release (blocking; run on a worker).
fn fetch_latest(url: Option<String>) -> Result<CheckResult, String> {
    let url = url.or_else(|| std::env::var("DBIJI_UPDATE_URL").ok()).unwrap_or_else(|| RELEASES_API.to_string());
    if url == "off" {
        return Ok(CheckResult::UpToDate);
    }
    let crypto = std::sync::Arc::new(rustls::crypto::aws_lc_rs::default_provider());
    let tls = ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::Rustls).root_certs(ureq::tls::RootCerts::WebPki).unversioned_rustls_crypto_provider(crypto).build();
    let agent: ureq::Agent = ureq::Agent::config_builder()
        .tls_config(tls)
        .http_status_as_error(false)
        .timeout_global(Some(Duration::from_secs(15)))
        .user_agent(format!("DBiji/{}", current_version()))
        .build()
        .into();
    let mut resp = agent.get(&url).header("Accept", "application/vnd.github+json").call().map_err(|e| e.to_string())?;
    match resp.status().as_u16() {
        200 => {}
        // No release published yet.
        404 => return Ok(CheckResult::UpToDate),
        s => return Err(format!("HTTP {s}")),
    }
    let body = resp.body_mut().with_config().limit(2 * 1024 * 1024).read_to_string().map_err(|e| e.to_string())?;
    let release = parse_release(&body).ok_or_else(|| "unexpected response".to_string())?;
    Ok(if is_newer(&release.version, current_version()) { CheckResult::Available(release) } else { CheckResult::UpToDate })
}

/// The first start ever (no settings or connections yet) shows no notes.
pub fn is_existing_install(paths: &dbiji_config::AppPaths) -> bool {
    paths.settings_file().exists() || paths.connections_file().exists()
}

/// Popups and the background check.
#[derive(Default)]
pub struct Updates {
    /// "What's new" notes being shown.
    pub whats_new: Option<Vec<Notes>>,
    /// Asking whether to check automatically.
    pub ask: bool,
    /// A newer release being offered (the update dialog).
    pub available: Option<Release>,
    /// A newer release found by a check (the toolbar's Update button).
    pub found: Option<Release>,
    /// Data folder: where the installer leaves the outcome of an update.
    pub data_dir: Option<std::path::PathBuf>,
    /// Quit now: the update is installed once DBiji has quit.
    pub restart: bool,
    /// "Restart" was chosen: the installer starts when DBiji quits (not before the
    /// quit is confirmed, and once).
    #[cfg(target_os = "macos")]
    install_on_quit: bool,
    check: Option<(Job<Result<CheckResult, String>>, bool)>,
    /// Set by `start`: the next frame decides what to show.
    started: bool,
    /// The check started at launch has not answered yet: a new version
    /// opens the dialog (later checks only light up the toolbar button).
    launch_check: bool,
    /// The version already announced by a toast.
    announced: Option<String>,
    /// Next automatic check while DBiji stays open.
    next_check: Option<std::time::Instant>,
    /// The Update button was clicked: ask again, so the offer on screen is
    /// the newest release, not the one found hours ago.
    refresh: bool,
    /// Where the latest release is read (tests); default `DBIJI_UPDATE_URL`, else GitHub.
    api_url: Option<String>,
    /// The outcome of the previous run's update was reported.
    #[cfg(target_os = "macos")]
    reported: bool,
    #[cfg(target_os = "macos")]
    install: Install,
    /// The install dialog (progress, ready, error) is on screen.
    #[cfg(target_os = "macos")]
    install_visible: bool,
}

/// Installing an update (macOS).
#[cfg(target_os = "macos")]
#[derive(Default)]
enum Install {
    #[default]
    Idle,
    Downloading {
        release: Release,
        stage: std::sync::Arc<parking_lot::Mutex<crate::selfupdate::Stage>>,
        job: Job<Result<crate::selfupdate::Prepared, String>>,
    },
    Ready {
        release: Release,
        prepared: crate::selfupdate::Prepared,
    },
    Failed(String),
}

/// Automatic checks while DBiji stays open (one also runs at every start).
const CHECK_EVERY: Duration = Duration::from_secs(6 * 60 * 60);

fn now_secs() -> i64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0)
}

/// The update can be installed in place (macOS, DBiji running from an
/// installed `DBiji.app`, the release has its archive).
fn installable(r: &Release) -> bool {
    #[cfg(target_os = "macos")]
    {
        r.archive.is_some() && crate::selfupdate::installed_bundle().is_some()
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = r;
        false
    }
}

impl Updates {
    /// At startup: notes of an update, the one-time question, the check.
    pub fn start(&mut self, backend: &Backend, settings: &mut Settings, existing_install: bool) -> bool {
        self.started = true;
        let current = current_version();
        let seen = settings.updates.last_seen_version.clone();
        let mut changed = false;
        if seen.as_deref().is_none_or(|s| is_newer(current, s)) {
            if existing_install {
                // Installed before notes were tracked: show this version's notes.
                let since = seen.as_deref().or(Some("0"));
                let notes = notes_since(CHANGELOG, if seen.is_some() { since } else { None }, current);
                let notes: Vec<Notes> = if seen.is_some() { notes } else { notes.into_iter().take(1).collect() };
                if !notes.is_empty() {
                    self.whats_new = Some(notes);
                }
            }
            settings.updates.last_seen_version = Some(current.to_string());
            changed = true;
        }
        match settings.updates.check_automatically {
            None => self.ask = existing_install,
            Some(true) => {
                settings.updates.last_check = Some(now_secs());
                changed = true;
                self.launch_check = true;
                self.check_now(backend, false);
            }
            Some(false) => {}
        }
        changed
    }

    /// Show the notes of this version (Help menu).
    pub fn show_whats_new(&mut self) {
        let notes = notes_since(CHANGELOG, None, current_version());
        self.whats_new = Some(notes.into_iter().take(1).collect());
    }

    /// Check now, always asking for the latest release (one found earlier
    /// may have been followed by newer ones); `manual` shows the result,
    /// "up to date" and errors included.
    pub fn check_now(&mut self, backend: &Backend, manual: bool) {
        if let Some((_, running_manual)) = &mut self.check {
            *running_manual |= manual;
            return;
        }
        let url = self.api_url.clone();
        let job = backend.spawn(async move { tokio::task::spawn_blocking(move || fetch_latest(url)).await.unwrap_or_else(|e| Err(e.to_string())) });
        self.check = Some((job, manual));
    }

    /// A release newer than the update being installed replaces it: the
    /// install starts over from the offer. True when one was dropped.
    #[cfg(target_os = "macos")]
    fn drop_outdated_install(&mut self, newest: &Release) -> bool {
        let outdated = match &self.install {
            Install::Downloading { release, .. } | Install::Ready { release, .. } => is_newer(&newest.version, &release.version),
            Install::Failed(_) => true,
            Install::Idle => false,
        };
        if outdated {
            if let Install::Ready { prepared, .. } = std::mem::take(&mut self.install) {
                let _ = std::fs::remove_dir_all(&prepared.dir);
            }
            self.install_visible = false;
        }
        outdated
    }

    /// A check is running.
    pub fn checking(&self) -> bool {
        self.check.is_some()
    }

    /// Show what there is to show about a found release: the install
    /// progress / ready dialog when it is being installed, else the offer.
    fn show_found(&mut self, r: Release) {
        #[cfg(target_os = "macos")]
        if !matches!(self.install, Install::Idle) {
            self.install_visible = true;
            return;
        }
        self.available = Some(r);
    }

    /// The toolbar button while an update is available, downloading or
    /// ready to install.
    pub fn toolbar_button(&mut self, ui: &mut egui::Ui) {
        let label = match self.found.as_ref() {
            None => return,
            Some(r) => format!("{} {} {}", icon::ARROW_CIRCLE_UP, tr("Update"), r.version),
        };
        #[cfg(target_os = "macos")]
        let label = match &self.install {
            Install::Downloading { stage, .. } => match *stage.lock() {
                crate::selfupdate::Stage::Downloading { done, total } if total > 0 => format!("{} {}%", icon::ARROW_CIRCLE_UP, done * 100 / total),
                _ => format!("{} {}", icon::ARROW_CIRCLE_UP, tr("Update")),
            },
            Install::Ready { .. } => format!("{} {}", icon::ARROW_CIRCLE_UP, tr("Restart to update")),
            _ => label,
        };
        let accent = crate::theme::accent(ui.visuals().dark_mode);
        let text = egui::Color32::WHITE;
        let b = egui::Button::new(RichText::new(label).color(text).strong()).fill(accent);
        if ui.add(b).on_hover_text(tr("A new version of DBiji is available")).clicked()
            && let Some(r) = self.found.clone()
        {
            self.show_found(r);
            self.refresh = true;
        }
    }

    /// DBiji is quitting: start the installer of the update chosen with "Restart".
    pub fn install_on_exit(&mut self) {
        #[cfg(target_os = "macos")]
        if std::mem::take(&mut self.install_on_quit)
            && let Install::Ready { prepared, .. } = &self.install
            && let (Some(bundle), Some(dir)) = (crate::selfupdate::installed_bundle(), &self.data_dir)
            && let Err(e) = crate::selfupdate::launch_installer(prepared, &bundle, dir)
        {
            tracing::warn!("the update installer did not start: {e}");
        }
    }

    /// Draw the popups; returns true when settings changed. Sets
    /// [`Updates::restart`] when DBiji must quit for the installer.
    pub fn ui(&mut self, ctx: &Context, backend: &Backend, settings: &mut Settings, toasts: &mut Toasts) -> bool {
        let mut changed = false;
        #[cfg(target_os = "macos")]
        self.report_previous_update(toasts);
        if std::mem::take(&mut self.refresh) {
            self.check_now(backend, false);
        }
        if let Some((job, manual)) = &mut self.check {
            let manual = *manual;
            if let Some(result) = job.poll() {
                self.check = None;
                let at_launch = std::mem::take(&mut self.launch_check);
                match result {
                    Ok(CheckResult::Available(r)) if manual || settings.updates.skipped_version.as_deref() != Some(r.version.as_str()) => {
                        self.found = Some(r.clone());
                        #[cfg(target_os = "macos")]
                        let dropped = self.drop_outdated_install(&r);
                        #[cfg(not(target_os = "macos"))]
                        let dropped = false;
                        if manual || at_launch || dropped {
                            self.show_found(r);
                        } else if self.available.is_some() {
                            // The offer on screen shows the newest release.
                            self.available = Some(r);
                        } else if self.announced.as_deref() != Some(r.version.as_str()) {
                            toasts.info(trf("DBiji {} is available: use the Update button to install it.", &[&r.version]));
                        }
                        self.announced = self.found.as_ref().map(|r| r.version.clone());
                    }
                    Ok(_) if manual => toasts.success(trf("DBiji {} is the latest version", &[&current_version()])),
                    Ok(_) => {}
                    Err(e) if manual => toasts.error(tr("Cannot check for updates"), Some(e)),
                    Err(e) => tracing::info!("update check failed: {e}"),
                }
            } else {
                ctx.request_repaint_after(Duration::from_millis(200));
            }
        }
        // Automatic checks while DBiji stays open.
        if settings.updates.check_automatically == Some(true) && self.started {
            let now = std::time::Instant::now();
            let next = *self.next_check.get_or_insert(now + CHECK_EVERY);
            if now >= next && self.check.is_none() {
                self.next_check = Some(now + CHECK_EVERY);
                settings.updates.last_check = Some(now_secs());
                changed = true;
                self.check_now(backend, false);
            } else {
                ctx.request_repaint_after((next.saturating_duration_since(now)).min(Duration::from_secs(600)));
            }
        }
        if let Some(notes) = &self.whats_new {
            let mut close = false;
            let modal = egui::Modal::new(egui::Id::new("dbiji_whats_new")).show(ctx, |ui| {
                ui.set_width(560.0);
                crate::widgets::icon_heading(ui, icon::SPARKLE, trf("What's new in DBiji {}", &[&current_version()]));
                ui.add_space(6.0);
                egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.6).show(ui, |ui| {
                    for n in notes {
                        if notes.len() > 1 || !n.date.is_empty() {
                            ui.label(RichText::new(format!("{}  {}", n.version, n.date)).strong().weak());
                        }
                        markdown(ui, &n.body);
                        ui.add_space(8.0);
                    }
                });
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.link(tr("All release notes")).clicked() {
                        ctx.open_url(egui::OpenUrl::new_tab(RELEASES_PAGE));
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.button(RichText::new(tr("Continue")).strong()).clicked() {
                            close = true;
                        }
                    });
                });
            });
            if close || modal.should_close() {
                self.whats_new = None;
            }
            return changed;
        }
        #[cfg(target_os = "macos")]
        if self.install_ui(ctx, backend) {
            return changed;
        }
        if let Some(r) = self.available.clone() {
            let mut close = false;
            let can_install = installable(&r);
            let modal = egui::Modal::new(egui::Id::new("dbiji_update_available")).show(ctx, |ui| {
                ui.set_width(560.0);
                crate::widgets::icon_heading(ui, icon::ARROW_CIRCLE_UP, tr("A new version of DBiji is available"));
                ui.add_space(4.0);
                egui::Grid::new("dbiji_update_versions").num_columns(2).spacing([16.0, 4.0]).show(ui, |ui| {
                    ui.label(tr("Current version:"));
                    ui.label(current_version());
                    ui.end_row();
                    ui.label(tr("New version:"));
                    let new = match &r.date {
                        Some(d) => format!("{}   ({d})", r.version),
                        None => r.version.clone(),
                    };
                    ui.label(RichText::new(new).strong());
                    ui.end_row();
                });
                if !r.notes.is_empty() {
                    ui.add_space(6.0);
                    ui.label(RichText::new(tr("What's new:")).weak());
                    egui::ScrollArea::vertical().max_height(ctx.content_rect().height() * 0.45).show(ui, |ui| markdown(ui, &r.notes));
                }
                if !can_install && cfg!(target_os = "macos") {
                    ui.add_space(4.0);
                    ui.label(RichText::new(tr("Installing it from here needs DBiji in the Applications folder; download the new version instead.")).weak().small());
                }
                ui.weak(tr("Homebrew: brew upgrade --cask dbiji"));
                ui.separator();
                ui.horizontal(|ui| {
                    if ui.button(tr("Skip this version")).clicked() {
                        settings.updates.skipped_version = Some(r.version.clone());
                        self.found = None;
                        changed = true;
                        close = true;
                    }
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        let accent = crate::theme::accent(ui.visuals().dark_mode);
                        let on_accent = egui::Color32::WHITE;
                        if can_install {
                            let b = egui::Button::new(RichText::new(format!("{} {}", icon::ARROW_CIRCLE_UP, tr("Install update"))).color(on_accent).strong()).fill(accent);
                            if ui.add(b).clicked() {
                                #[cfg(target_os = "macos")]
                                self.start_install(backend, r.clone());
                                close = true;
                            }
                        } else if ui.button(RichText::new(format!("{} {}", icon::DOWNLOAD_SIMPLE, tr("Download"))).strong()).clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(r.download_url.clone().unwrap_or_else(|| r.url.clone())));
                            close = true;
                        }
                        if ui.button(tr("Remind me later")).clicked() {
                            close = true;
                        }
                        if ui.button(tr("View on GitHub")).clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(r.url.clone()));
                        }
                    });
                });
            });
            if close || modal.should_close() {
                self.available = None;
            }
            return changed;
        }
        if self.ask && self.started {
            let mut answer: Option<bool> = None;
            egui::Area::new(egui::Id::new("dbiji_update_question")).anchor(egui::Align2::RIGHT_BOTTOM, egui::vec2(-16.0, -40.0)).order(egui::Order::Foreground).show(ctx, |ui| {
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.set_max_width(360.0);
                    ui.label(RichText::new(format!("{} {}", icon::ARROW_CIRCLE_UP, tr("Check for updates automatically?"))).strong());
                    ui.label(tr("DBiji can look for a new release when it starts and every few hours. Only the GitHub release page is contacted; nothing about you or your databases is sent."));
                    ui.horizontal(|ui| {
                        if ui.button(RichText::new(tr("Check automatically")).strong()).clicked() {
                            answer = Some(true);
                        }
                        if ui.button(tr("Don't check")).clicked() {
                            answer = Some(false);
                        }
                    });
                });
            });
            if let Some(a) = answer {
                self.ask = false;
                settings.updates.check_automatically = Some(a);
                changed = true;
            }
        }
        changed
    }

    /// The outcome the installer left for this launch (once), and the
    /// leftovers of an interrupted update.
    #[cfg(target_os = "macos")]
    fn report_previous_update(&mut self, toasts: &mut Toasts) {
        if std::mem::replace(&mut self.reported, true) || !self.started {
            return;
        }
        std::thread::spawn(crate::selfupdate::remove_leftovers);
        let Some(dir) = &self.data_dir else { return };
        match crate::selfupdate::take_outcome(dir) {
            Some(crate::selfupdate::Outcome::Installed(v)) if v == current_version() => toasts.success(trf("DBiji was updated to version {}", &[&v])),
            Some(crate::selfupdate::Outcome::Installed(_)) | None => {}
            Some(crate::selfupdate::Outcome::Cancelled) => {
                self.install = Install::Failed(tr("The update was not installed because no administrator password was entered.").to_string());
                self.install_visible = true;
            }
            Some(crate::selfupdate::Outcome::Failed(reason)) => {
                self.install = Install::Failed(trf("The update could not be installed: {}", &[&reason]));
                self.install_visible = true;
            }
        }
    }

    /// Download and check the release's archive in the background.
    #[cfg(target_os = "macos")]
    fn start_install(&mut self, backend: &Backend, release: Release) {
        let Some(asset) = release.archive.clone() else { return };
        let stage = std::sync::Arc::new(parking_lot::Mutex::new(crate::selfupdate::Stage::Starting));
        let (st, version) = (stage.clone(), release.version.clone());
        let job =
            backend.spawn(async move { tokio::task::spawn_blocking(move || crate::selfupdate::download(&version, &asset, &st)).await.unwrap_or_else(|e| Err(e.to_string())) });
        self.install = Install::Downloading { release, stage, job };
        self.install_visible = true;
    }

    /// Progress, "ready to install" and errors of an update; true when a
    /// dialog is on screen.
    #[cfg(target_os = "macos")]
    fn install_ui(&mut self, ctx: &Context, backend: &Backend) -> bool {
        let _ = backend;
        if let Install::Downloading { job, .. } = &mut self.install {
            match job.poll() {
                Some(Ok(prepared)) => {
                    let Install::Downloading { release, .. } = std::mem::take(&mut self.install) else { unreachable!() };
                    self.install = Install::Ready { release, prepared };
                    self.install_visible = true;
                }
                Some(Err(e)) => {
                    self.install = Install::Failed(trf("The update could not be installed: {}", &[&e]));
                    self.install_visible = true;
                }
                None => ctx.request_repaint_after(Duration::from_millis(200)),
            }
        }
        if !self.install_visible || matches!(self.install, Install::Idle) {
            return false;
        }
        let mut hide = false;
        let mut restart = false;
        let mut dismiss = false;
        let modal = egui::Modal::new(egui::Id::new("dbiji_update_install")).show(ctx, |ui| {
            ui.set_width(460.0);
            match &self.install {
                Install::Downloading { release, stage, .. } => {
                    ui.heading(trf("Updating to DBiji {}", &[&release.version]));
                    ui.add_space(6.0);
                    let (fraction, text) = match stage.lock().clone() {
                        crate::selfupdate::Stage::Starting => (0.0, tr("Starting…").to_string()),
                        crate::selfupdate::Stage::Downloading { done, total } => (
                            if total > 0 { done as f32 / total as f32 } else { 0.0 },
                            trf("Downloading… {} of {}", &[&crate::widgets::format_bytes(done as i64), &crate::widgets::format_bytes(total as i64)]),
                        ),
                        crate::selfupdate::Stage::Verifying => (1.0, tr("Verifying…").to_string()),
                    };
                    ui.add(egui::ProgressBar::new(fraction));
                    ui.weak(text);
                    ui.add_space(8.0);
                    if ui.button(tr("Hide")).on_hover_text(tr("The download goes on; the toolbar shows its progress")).clicked() {
                        hide = true;
                    }
                }
                Install::Ready { release, .. } => {
                    ui.heading(trf("DBiji {} is ready to install", &[&release.version]));
                    ui.label(tr("DBiji will quit, install the new version and open again. You are asked about unsaved scripts first."));
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        let accent = crate::theme::accent(ui.visuals().dark_mode);
                        let on_accent = egui::Color32::WHITE;
                        if ui.add(egui::Button::new(RichText::new(tr("Restart now")).color(on_accent).strong()).fill(accent)).clicked() {
                            restart = true;
                        }
                        if ui.button(tr("Later")).clicked() {
                            hide = true;
                        }
                    });
                }
                Install::Failed(e) => {
                    ui.heading(tr("Software update"));
                    ui.colored_label(ui.visuals().error_fg_color, e.as_str());
                    ui.add_space(8.0);
                    ui.horizontal(|ui| {
                        if ui.button(tr("OK")).clicked() {
                            dismiss = true;
                        }
                        if ui.button(tr("Open the releases page")).clicked() {
                            ctx.open_url(egui::OpenUrl::new_tab(RELEASES_PAGE));
                        }
                    });
                }
                Install::Idle => {}
            }
        });
        if modal.should_close() {
            hide = true;
        }
        if restart && let Install::Ready { prepared, .. } = &self.install {
            let ready = match (crate::selfupdate::installed_bundle(), &self.data_dir) {
                (Some(bundle), Some(_)) => crate::selfupdate::check_installable(prepared, &bundle),
                _ => Err(tr("DBiji is not installed as an app").to_string()),
            };
            match ready {
                Ok(()) => {
                    self.install_on_quit = true;
                    self.restart = true;
                    hide = true;
                }
                Err(e) => self.install = Install::Failed(trf("The update could not be installed: {}", &[&e])),
            }
        }
        if dismiss || (hide && matches!(self.install, Install::Failed(_))) {
            self.install = Install::Idle;
        }
        if hide || dismiss {
            self.install_visible = false;
        }
        true
    }
}

/// Minimal Markdown: headings, bullets, `code` and **bold** markers.
fn markdown(ui: &mut egui::Ui, text: &str) {
    for line in text.lines() {
        let t = line.trim_end();
        let plain = |s: &str| s.replace("**", "").replace('`', "");
        if t.trim().is_empty() {
            ui.add_space(4.0);
        } else if let Some(h) = t.strip_prefix("### ").or_else(|| t.strip_prefix("## ")).or_else(|| t.strip_prefix("# ")) {
            ui.add_space(2.0);
            ui.label(RichText::new(plain(h)).strong());
        } else if let Some(b) = t.trim_start().strip_prefix("- ").or_else(|| t.trim_start().strip_prefix("* ")) {
            let indent = (t.len() - t.trim_start().len()) as f32 * 4.0;
            ui.horizontal_wrapped(|ui| {
                ui.add_space(indent);
                ui.label("•");
                ui.label(plain(b));
            });
        } else {
            ui.label(plain(t));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LOG: &str = "# Changelog\n\nIntro.\n\n## 0.3.0 — 2026-12-01\n\n### Added\n- C\n\n## [0.2.0] - 2026-10-18\n\n- B\n\n## 0.1.0\n\n- A\n";

    #[test]
    fn versions() {
        assert!(is_newer("0.2.0", "0.1.9"));
        assert!(is_newer("v1.10", "1.9.3"));
        assert!(!is_newer("0.2.0", "0.2"));
        assert!(is_newer("0.2.0", "0.2.0-beta.1"));
        assert!(!is_newer("0.2.0-rc1", "0.2.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
    }

    #[test]
    fn changelog_sections() {
        let n = parse_changelog(LOG);
        assert_eq!(n.iter().map(|n| n.version.as_str()).collect::<Vec<_>>(), ["0.3.0", "0.2.0", "0.1.0"]);
        assert_eq!(n[0].date, "2026-12-01");
        assert_eq!(n[1].date, "2026-10-18");
        assert_eq!(n[0].body, "### Added\n- C");
        // updating from 0.1.0 to 0.3.0 shows 0.3.0 and 0.2.0; a future entry is not shown
        let since = notes_since(LOG, Some("0.1.0"), "0.3.0");
        assert_eq!(since.iter().map(|n| n.version.as_str()).collect::<Vec<_>>(), ["0.3.0", "0.2.0"]);
        assert_eq!(notes_since(LOG, Some("0.1.0"), "0.2.0").len(), 1);
        assert!(notes_since(LOG, Some("0.3.0"), "0.3.0").is_empty());
        // the built-in changelog has this version
        assert!(parse_changelog(CHANGELOG).iter().any(|n| n.version == current_version()), "CHANGELOG.md lists {}", current_version());
    }

    #[test]
    fn release_json() {
        let json = r####"{"tag_name":"v0.3.0","html_url":"https://github.com/ilhamnadhif/dbiji/releases/tag/v0.3.0","draft":false,"prerelease":false,
            "published_at":"2026-12-01T08:00:00Z","body":"### Fixed\r\n- Things","assets":[
            {"name":"DBiji-0.3.0.zip","size":1234,"digest":"sha256:ABCD","browser_download_url":"https://x/DBiji-0.3.0.zip"},
            {"name":"DBiji-0.3.0.dmg","size":2345,"browser_download_url":"https://x/DBiji-0.3.0.dmg"}]}"####;
        let r = parse_release(json).unwrap();
        assert_eq!(r.version, "0.3.0");
        assert_eq!(r.download_url.as_deref(), Some("https://x/DBiji-0.3.0.dmg"));
        assert_eq!(r.notes, "### Fixed\n- Things");
        assert_eq!(r.date.as_deref(), Some("2026-12-01"));
        let a = r.archive.unwrap();
        assert_eq!((a.name.as_str(), a.url.as_str(), a.size, a.sha256.as_deref()), ("DBiji-0.3.0.zip", "https://x/DBiji-0.3.0.zip", 1234, Some("abcd")));
        // no archive for this version: download only
        let other = json.replace("DBiji-0.3.0.zip", "DBiji-0.2.0.zip");
        assert!(parse_release(&other).unwrap().archive.is_none());
        assert!(parse_release(r#"{"tag_name":"v9","prerelease":true}"#).is_none());
        assert!(parse_release("not json").is_none());
    }

    /// An HTTP server answering every request with the release `latest` names.
    fn release_server(latest: std::sync::Arc<parking_lot::Mutex<String>>) -> String {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/releases/latest", listener.local_addr().unwrap());
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                let _ = stream.read(&mut [0u8; 4096]);
                let body = format!(r#"{{"tag_name":"v{}","draft":false,"prerelease":false,"assets":[]}}"#, latest.lock());
                let _ = write!(stream, "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len());
            }
        });
        url
    }

    /// Frames until the running check answered.
    fn settle(u: &mut Updates, ctx: &Context, backend: &Backend, settings: &mut Settings, toasts: &mut Toasts) {
        for _ in 0..1000 {
            let mut out = ctx.run_ui(egui::RawInput::default(), |ui| {
                u.ui(ui.ctx(), backend, settings, toasts);
            });
            out.textures_delta.clear();
            if !u.checking() && !u.refresh {
                return;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("the update check did not answer");
    }

    /// A release found earlier is not offered once a newer one is out:
    /// Check for Updates… and the Update button ask again (0.3.8 was
    /// offered 0.3.9 although 0.3.10 had been published).
    #[test]
    fn the_newest_release_is_offered() {
        let backend = Backend::new().unwrap();
        let latest = std::sync::Arc::new(parking_lot::Mutex::new("90.0.0".to_string()));
        let mut u = Updates { api_url: Some(release_server(latest.clone())), ..Default::default() };
        let (ctx, mut settings, mut toasts) = (Context::default(), Settings::default(), Toasts::default());
        let version = |r: &Option<Release>| r.as_ref().map(|r| r.version.clone());
        // A background check finds 90.0.0: the toolbar's Update button.
        u.check_now(&backend, false);
        settle(&mut u, &ctx, &backend, &mut settings, &mut toasts);
        assert_eq!(version(&u.found).as_deref(), Some("90.0.0"));
        assert!(u.available.is_none());
        // 90.1.0 is published: Check for Updates… asks again.
        *latest.lock() = "90.1.0".into();
        u.check_now(&backend, true);
        settle(&mut u, &ctx, &backend, &mut settings, &mut toasts);
        assert_eq!(version(&u.available).as_deref(), Some("90.1.0"));
        assert_eq!(version(&u.found).as_deref(), Some("90.1.0"));
        // 90.2.0 while the offer is on screen: a click on Update asks again and the offer follows.
        *latest.lock() = "90.2.0".into();
        u.refresh = true;
        settle(&mut u, &ctx, &backend, &mut settings, &mut toasts);
        assert_eq!(version(&u.available).as_deref(), Some("90.2.0"));
        assert_eq!(version(&u.found).as_deref(), Some("90.2.0"));
    }

    #[test]
    fn start_decides_what_to_show() {
        let backend = Backend::new().unwrap();
        // fresh install: nothing shown, nothing asked, version recorded
        let mut s = Settings::default();
        let mut u = Updates::default();
        assert!(u.start(&backend, &mut s, false));
        assert!(u.whats_new.is_none() && !u.ask);
        assert_eq!(s.updates.last_seen_version.as_deref(), Some(current_version()));
        // existing install that never saw notes: this version's notes and the question
        let mut s = Settings::default();
        let mut u = Updates::default();
        u.start(&backend, &mut s, true);
        assert_eq!(u.whats_new.as_ref().map(|n| n[0].version.clone()).as_deref(), Some(current_version()));
        assert!(u.ask);
        // same version again: nothing
        let mut u = Updates::default();
        s.updates.check_automatically = Some(false);
        assert!(!u.start(&backend, &mut s, true));
        assert!(u.whats_new.is_none() && !u.ask && !u.checking());
        // checking automatically: every start checks (the 6-hourly ones follow while open)
        s.updates.check_automatically = Some(true);
        s.updates.last_check = Some(now_secs());
        let mut u = Updates::default();
        u.start(&backend, &mut s, true);
        assert!(u.checking() && u.launch_check);
    }
}
