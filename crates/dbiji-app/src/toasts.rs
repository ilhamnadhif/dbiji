//! Transient notifications shown in the bottom-right corner.

use crate::theme;
use dbiji_core::tr;
use egui::{Align2, Context, Id, RichText};
use egui_phosphor::regular as icon;
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToastKind {
    Info,
    Success,
    Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Toast {
    pub kind: ToastKind,
    pub title: String,
    pub detail: Option<String>,
    created: Instant,
    duration: Duration,
}

#[derive(Default)]
pub struct Toasts {
    items: Vec<Toast>,
    /// Every toast since the last [`Toasts::take_log`] (bounded), for tests.
    log: Vec<Toast>,
}

impl Toasts {
    pub fn push(&mut self, kind: ToastKind, title: impl Into<String>, detail: Option<String>) {
        let duration = match kind {
            ToastKind::Error => Duration::from_secs(10),
            ToastKind::Warning => Duration::from_secs(7),
            _ => Duration::from_secs(4),
        };
        let toast = Toast { kind, title: title.into(), detail, created: Instant::now(), duration };
        if self.log.len() < 500 {
            self.log.push(toast.clone());
        }
        self.items.push(toast);
        if self.items.len() > 6 {
            self.items.remove(0);
        }
    }

    /// Takes the toasts pushed since the previous call.
    pub fn take_log(&mut self) -> Vec<Toast> {
        std::mem::take(&mut self.log)
    }

    pub fn info(&mut self, title: impl Into<String>) {
        self.push(ToastKind::Info, title, None);
    }
    pub fn success(&mut self, title: impl Into<String>) {
        self.push(ToastKind::Success, title, None);
    }
    pub fn warning(&mut self, title: impl Into<String>, detail: Option<String>) {
        self.push(ToastKind::Warning, title, detail);
    }
    pub fn error(&mut self, title: impl Into<String>, detail: Option<String>) {
        self.push(ToastKind::Error, title, detail);
    }

    pub fn show(&mut self, ctx: &Context) {
        let now = Instant::now();
        self.items.retain(|t| now.duration_since(t.created) < t.duration);
        if self.items.is_empty() {
            return;
        }
        let dark = ctx.theme() == egui::Theme::Dark;
        let mut close: Option<usize> = None;
        egui::Area::new(Id::new("dbiji_toasts")).anchor(Align2::RIGHT_BOTTOM, egui::vec2(-12.0, -34.0)).order(egui::Order::Foreground).show(ctx, |ui| {
            ui.set_max_width(380.0);
            for (i, t) in self.items.iter().enumerate().rev() {
                let (glyph, color) = match t.kind {
                    ToastKind::Info => (icon::INFO, ui.visuals().hyperlink_color),
                    ToastKind::Success => (icon::CHECK_CIRCLE, theme::success_color(dark)),
                    ToastKind::Warning => (icon::WARNING, theme::warning_color(dark)),
                    ToastKind::Error => (icon::X_CIRCLE, theme::error_color(dark)),
                };
                egui::Frame::popup(ui.style()).show(ui, |ui| {
                    ui.horizontal_top(|ui| {
                        ui.label(RichText::new(glyph).color(color).size(16.0));
                        ui.vertical(|ui| {
                            ui.label(RichText::new(&t.title).strong());
                            if let Some(d) = &t.detail {
                                let short: String = d.chars().take(600).collect();
                                ui.label(RichText::new(short).small());
                            }
                        });
                        if ui.add(egui::Button::new(icon::X).frame(false)).on_hover_text(tr("Dismiss")).clicked() {
                            close = Some(i);
                        }
                    });
                });
                ui.add_space(4.0);
            }
        });
        if let Some(i) = close {
            self.items.remove(i);
        }
        // Wake up again when the next toast expires (no busy repainting).
        if let Some(next) = self.items.iter().map(|t| t.duration.saturating_sub(now.duration_since(t.created))).min() {
            ctx.request_repaint_after(next + Duration::from_millis(20));
        }
    }
}
