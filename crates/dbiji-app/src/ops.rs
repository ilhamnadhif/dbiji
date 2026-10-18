//! Long running background operations (exports, imports, compares, backups,
//! fetch-all ...). Every operation shows up in the operations panel with
//! progress and a cancel button (PRD §10: "every long running operation is
//! shown in the background task panel and can be cancelled").

use dbiji_core::{tr, trf};
use egui::{RichText, Ui};
use egui_phosphor::regular as icon;
use parking_lot::Mutex;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;
use tokio_util::sync::CancellationToken;

/// Progress shared between the background task and the UI.
#[derive(Default)]
pub struct OpProgress {
    pub done: AtomicU64,
    /// 0 = unknown
    pub total: AtomicU64,
    pub message: Mutex<String>,
}

impl OpProgress {
    pub fn set(&self, done: u64, total: Option<u64>) {
        self.done.store(done, Ordering::Relaxed);
        if let Some(t) = total {
            self.total.store(t, Ordering::Relaxed);
        }
    }
    pub fn set_message(&self, m: impl Into<String>) {
        *self.message.lock() = m.into();
    }
}

/// Handle given to the task performing the work.
#[derive(Clone)]
pub struct OpHandle {
    pub id: u64,
    pub progress: Arc<OpProgress>,
    pub cancel: CancellationToken,
}

#[derive(Debug, Clone)]
pub enum OpState {
    Running,
    Succeeded(String),
    Failed(String),
    Cancelled,
}

pub struct Operation {
    pub id: u64,
    pub title: String,
    pub started: Instant,
    pub finished: Option<Instant>,
    pub progress: Arc<OpProgress>,
    pub cancel: CancellationToken,
    pub state: OpState,
}

#[derive(Default)]
pub struct Operations {
    list: Vec<Operation>,
    next_id: u64,
    pub panel_open: bool,
}

impl Operations {
    pub fn start(&mut self, title: impl Into<String>) -> OpHandle {
        self.next_id += 1;
        let progress = Arc::new(OpProgress::default());
        let cancel = CancellationToken::new();
        self.list.push(Operation {
            id: self.next_id,
            title: title.into(),
            started: Instant::now(),
            finished: None,
            progress: progress.clone(),
            cancel: cancel.clone(),
            state: OpState::Running,
        });
        if self.list.len() > 100 {
            // drop the oldest finished entries
            if let Some(pos) = self.list.iter().position(|o| !matches!(o.state, OpState::Running)) {
                self.list.remove(pos);
            }
        }
        OpHandle { id: self.next_id, progress, cancel }
    }

    pub fn finish(&mut self, id: u64, state: OpState) {
        if let Some(op) = self.list.iter_mut().find(|o| o.id == id) {
            op.state = state;
            op.finished = Some(Instant::now());
        }
    }

    pub fn running(&self) -> usize {
        self.list.iter().filter(|o| matches!(o.state, OpState::Running)).count()
    }

    pub fn cancel_all(&self) {
        for o in &self.list {
            if matches!(o.state, OpState::Running) {
                o.cancel.cancel();
            }
        }
    }

    /// Operations panel content.
    pub fn ui(&mut self, ui: &mut Ui) {
        if self.list.is_empty() {
            ui.weak(tr("No background operations."));
            return;
        }
        let mut clear = false;
        ui.horizontal(|ui| {
            ui.strong(trf("{} running", &[&self.running()]));
            if ui.small_button(tr("Clear finished")).clicked() {
                clear = true;
            }
        });
        ui.separator();
        egui::ScrollArea::vertical().max_height(260.0).show(ui, |ui| {
            for op in self.list.iter().rev() {
                ui.horizontal(|ui| {
                    let (glyph, txt) = match &op.state {
                        OpState::Running => (icon::HOURGLASS, String::new()),
                        OpState::Succeeded(m) => (icon::CHECK_CIRCLE, m.clone()),
                        OpState::Failed(m) => (icon::X_CIRCLE, m.clone()),
                        OpState::Cancelled => (icon::PROHIBIT, tr("Cancelled").to_string()),
                    };
                    ui.label(glyph);
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&op.title).strong());
                        let elapsed = op.finished.unwrap_or_else(Instant::now).duration_since(op.started);
                        let done = op.progress.done.load(Ordering::Relaxed);
                        let total = op.progress.total.load(Ordering::Relaxed);
                        if matches!(op.state, OpState::Running) {
                            if total > 0 {
                                ui.add(egui::ProgressBar::new((done as f32 / total as f32).clamp(0.0, 1.0)).desired_width(220.0).text(format!("{done} / {total}")));
                            } else {
                                ui.horizontal(|ui| {
                                    ui.spinner();
                                    if done > 0 {
                                        ui.label(done.to_string());
                                    }
                                });
                            }
                            let msg = op.progress.message.lock().clone();
                            if !msg.is_empty() {
                                ui.small(msg);
                            }
                        } else if !txt.is_empty() {
                            ui.small(txt);
                        }
                        ui.small(format!("{:.1}s", elapsed.as_secs_f32()));
                    });
                    if matches!(op.state, OpState::Running) && ui.button(icon::STOP).on_hover_text(tr("Cancel")).clicked() {
                        op.cancel.cancel();
                    }
                });
                ui.separator();
            }
        });
        if clear {
            self.list.retain(|o| matches!(o.state, OpState::Running));
        }
        if self.running() > 0 {
            ui.ctx().request_repaint_after(std::time::Duration::from_millis(250));
        }
    }
}
