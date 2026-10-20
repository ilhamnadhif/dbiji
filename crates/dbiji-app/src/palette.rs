//! Command palette (⇧⌘P): quick access to actions, connections, objects and scripts.

use crate::backend::Loadable;
use crate::command::{AppCommand, ObjectPage};
use crate::cx::AppContext;
use dbiji_core::tr;
use egui::{Key, RichText};
use egui_phosphor::regular as icon;

#[derive(Clone, Default)]
struct PaletteState {
    query: String,
    selected: usize,
    focus_requested: bool,
    /// Scripts and bookmarks of the project, read once per opening.
    project: Option<std::sync::Arc<(Vec<(String, std::path::PathBuf)>, Vec<dbiji_config::Bookmark>)>>,
}

enum Target {
    Script(std::path::PathBuf),
    Bookmark(String, dbiji_core::ObjectRef),
    Action(crate::keys::Action),
    Command(fn() -> AppCommand),
    Connect(String),
    Editor(String),
    Object(String, dbiji_core::ObjectRef),
}

struct Item {
    icon: &'static str,
    label: String,
    detail: String,
    target: Target,
}

/// Case-insensitive subsequence score; higher is better.
pub fn fuzzy(pattern: &str, text: &str) -> Option<i32> {
    if pattern.is_empty() {
        return Some(0);
    }
    let t: Vec<char> = text.to_lowercase().chars().collect();
    let mut score = 0i32;
    let mut ti = 0usize;
    let mut last: Option<usize> = None;
    for pc in pattern.to_lowercase().chars().filter(|c| !c.is_whitespace()) {
        let mut found = None;
        while ti < t.len() {
            if t[ti] == pc {
                found = Some(ti);
                ti += 1;
                break;
            }
            ti += 1;
        }
        let i = found?;
        score += 10;
        if i == 0 || matches!(t.get(i.wrapping_sub(1)), Some(' ' | '_' | '.' | '-' | ':' | '/')) {
            score += 8;
        }
        if last.is_some_and(|l| l + 1 == i) {
            score += 6;
        }
        last = Some(i);
    }
    // Typed as one piece: "set" ranks "Settings" above "Session Manager".
    let (text, pattern) = (text.to_lowercase(), pattern.trim().to_lowercase());
    if text.starts_with(&pattern) {
        score += 30;
    } else if text.contains(&pattern) {
        score += 15;
    }
    Some(score - t.len() as i32 / 4)
}

fn collect_project(cx: &AppContext, state: &mut PaletteState) -> Vec<Item> {
    let data = state
        .project
        .get_or_insert_with(|| {
            let p = cx.services.projects.as_ref().and_then(|m| m.default_project().ok());
            let scripts = p.as_ref().and_then(|p| p.list_scripts().ok()).unwrap_or_default().into_iter().map(|s| (s.relative, s.path)).collect();
            let bookmarks = p.as_ref().and_then(|p| p.bookmarks().ok()).unwrap_or_default();
            std::sync::Arc::new((scripts, bookmarks))
        })
        .clone();
    let mut items = Vec::new();
    for (name, path) in &data.0 {
        items.push(Item { icon: icon::FILE_SQL, label: name.clone(), detail: tr("Script").to_string(), target: Target::Script(path.clone()) });
    }
    for b in &data.1 {
        items.push(Item {
            icon: icon::BOOKMARK_SIMPLE,
            label: b.label.clone(),
            detail: tr("Bookmark").to_string(),
            target: Target::Bookmark(b.connection_id.clone(), b.object.clone()),
        });
    }
    items
}

fn collect(cx: &AppContext) -> Vec<Item> {
    let mut items = vec![
        Item { icon: icon::PLUS, label: tr("New connection").into(), detail: String::new(), target: Target::Command(|| AppCommand::NewConnection { kind: None, folder: None }) },
        Item {
            icon: icon::FILE_SQL,
            label: tr("New SQL editor").into(),
            detail: String::new(),
            target: Target::Command(|| AppCommand::NewSqlEditor { connection: None, schema: None, text: None }),
        },
        Item { icon: icon::GEAR, label: tr("Settings").into(), detail: String::new(), target: Target::Command(|| AppCommand::OpenSettings) },
        Item { icon: icon::CLOCK_COUNTER_CLOCKWISE, label: tr("Query history").into(), detail: String::new(), target: Target::Command(|| AppCommand::OpenHistory) },
        Item { icon: icon::LIST_CHECKS, label: tr("Tasks").into(), detail: String::new(), target: Target::Command(|| AppCommand::OpenTasks) },
        Item {
            icon: icon::GIT_DIFF,
            label: tr("Compare schemas").into(),
            detail: String::new(),
            target: Target::Command(|| AppCommand::OpenSchemaCompare { connection: None, schema: None }),
        },
        Item {
            icon: icon::ARROWS_LEFT_RIGHT,
            label: tr("Compare data").into(),
            detail: String::new(),
            target: Target::Command(|| AppCommand::OpenDataCompare { connection: None, table: None }),
        },
        Item { icon: icon::SPARKLE, label: tr("AI assistant").into(), detail: String::new(), target: Target::Command(|| AppCommand::OpenAiAssistant { connection: None }) },
        Item { icon: icon::DOWNLOAD_SIMPLE, label: tr("Import connections").into(), detail: String::new(), target: Target::Command(|| AppCommand::ImportConnections) },
        Item { icon: icon::SIDEBAR, label: tr("Toggle navigator").into(), detail: String::new(), target: Target::Command(|| AppCommand::ToggleNavigator) },
        Item { icon: icon::HOURGLASS, label: tr("Background operations").into(), detail: String::new(), target: Target::Command(|| AppCommand::ToggleOperations) },
        Item { icon: icon::KEYBOARD, label: tr("Keyboard shortcuts").into(), detail: String::new(), target: Target::Command(|| AppCommand::ShowShortcuts) },
        Item { icon: icon::PLUGS, label: tr("Disconnect all").into(), detail: String::new(), target: Target::Command(|| AppCommand::DisconnectAll) },
    ];
    for a in crate::keys::Action::ALL {
        let label = crate::keys::action_label(*a).to_string();
        let detail = format!("{} {}", tr(a.category().label()), crate::keys::label(cx.keymap, *a)).trim().to_string();
        // The same command listed above: it gets the category and shortcut.
        if let Some(it) = items.iter_mut().find(|it| it.label.to_lowercase() == label.to_lowercase()) {
            it.detail = detail;
            continue;
        }
        items.push(Item { icon: crate::keys::action_icon(*a), label, detail, target: Target::Action(*a) });
    }
    for c in cx.conns.configs() {
        let connected = cx.conns.is_connected(&c.id);
        if !connected {
            items.push(Item { icon: icon::PLUGS_CONNECTED, label: format!("{} {}", tr("Connect:"), c.name), detail: c.summary(), target: Target::Connect(c.id.clone()) });
        }
        items.push(Item { icon: icon::FILE_SQL, label: format!("{} {}", tr("SQL editor:"), c.name), detail: c.summary(), target: Target::Editor(c.id.clone()) });
    }
    for id in cx.conns.live_ids() {
        let Some(live) = cx.conns.live(&id) else { continue };
        for ((schema, kind), list) in &live.meta.objects {
            if let Loadable::Loaded(objs) = list {
                for o in objs.iter().take(5000) {
                    items.push(Item {
                        icon: crate::nav::kind_icon(*kind),
                        label: o.obj.name.clone(),
                        detail: format!("{} · {}", live.config.name, schema.schema.clone().unwrap_or_default()),
                        target: Target::Object(id.clone(), o.obj.clone()),
                    });
                }
            }
        }
    }
    items
}

/// Forget the previous query and selection (the palette is being opened).
pub fn reset(ctx: &egui::Context) {
    ctx.data_mut(|d| d.remove::<PaletteState>(egui::Id::new("dbiji_palette")));
}

/// One full-width row: the coloured icon, the label, and on the right the
/// detail (category and shortcut, connection, script folder).
fn row(ui: &mut egui::Ui, it: &Item, selected: bool) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 28.0), egui::Sense::click());
    let v = ui.visuals();
    if selected {
        ui.painter().rect_filled(rect, 4.0, v.selection.bg_fill);
    } else if resp.hovered() {
        ui.painter().rect_filled(rect, 4.0, v.widgets.hovered.weak_bg_fill);
    }
    let text = if selected { v.selection.stroke.color } else { v.text_color() };
    let weak = v.weak_text_color();
    let body = egui::TextStyle::Body.resolve(ui.style());
    ui.painter().text(rect.left_center() + egui::vec2(10.0, 0.0), egui::Align2::LEFT_CENTER, it.icon, body, dbiji_icons::color(it.icon).unwrap_or(weak));
    let fit = |text: RichText, width: f32, style: egui::TextStyle| egui::WidgetText::from(text).into_galley(ui, Some(egui::TextWrapMode::Truncate), width, style);
    let detail = (!it.detail.is_empty()).then(|| fit(RichText::new(&it.detail).small().color(weak), rect.width() * 0.45, egui::TextStyle::Small));
    let detail_w = detail.as_ref().map_or(0.0, |g| g.size().x + 16.0);
    let label = fit(RichText::new(&it.label).color(text), (rect.width() - 44.0 - detail_w).max(40.0), egui::TextStyle::Body);
    ui.painter().galley(egui::pos2(rect.left() + 34.0, rect.center().y - label.size().y / 2.0), label, text);
    if let Some(g) = detail {
        ui.painter().galley(egui::pos2(rect.right() - 10.0 - g.size().x, rect.center().y - g.size().y / 2.0), g, weak);
    }
    resp.widget_info(|| egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, true, selected, &it.label));
    resp
}

pub fn show(ctx: &egui::Context, cx: &mut AppContext, open: &mut bool) {
    let id = egui::Id::new("dbiji_palette");
    let mut state: PaletteState = ctx.data_mut(|d| d.get_temp(id)).unwrap_or_default();
    let mut items = collect(cx);
    items.extend(collect_project(cx, &mut state));
    let mut scored: Vec<(i32, &Item)> = items.iter().filter_map(|it| fuzzy(&state.query, &format!("{} {}", it.label, it.detail)).map(|s| (s, it))).collect();
    scored.sort_by_key(|s| std::cmp::Reverse(s.0));
    scored.truncate(60);
    if state.selected >= scored.len() {
        state.selected = scored.len().saturating_sub(1);
    }
    let mut chosen: Option<usize> = None;
    let (up, down, enter, esc) = ctx.input(|i| (i.key_pressed(Key::ArrowUp), i.key_pressed(Key::ArrowDown), i.key_pressed(Key::Enter), i.key_pressed(Key::Escape)));
    if esc {
        *open = false;
    }
    if down {
        state.selected = (state.selected + 1).min(scored.len().saturating_sub(1));
    }
    if up {
        state.selected = state.selected.saturating_sub(1);
    }
    if enter && !scored.is_empty() {
        chosen = Some(state.selected);
    }
    let area = egui::Area::new(id.with("area")).anchor(egui::Align2::CENTER_TOP, egui::vec2(0.0, 80.0)).order(egui::Order::Foreground).show(ctx, |ui| {
        egui::Frame::popup(ui.style()).show(ui, |ui| {
            ui.set_width(560.0);
            let r = ui.add(egui::TextEdit::singleline(&mut state.query).hint_text(tr("Type a command, connection or object…")).desired_width(f32::INFINITY));
            if !state.focus_requested {
                r.request_focus();
                state.focus_requested = true;
            }
            if r.changed() {
                state.selected = 0;
            }
            ui.separator();
            egui::ScrollArea::vertical().max_height(360.0).show(ui, |ui| {
                for (i, (_, it)) in scored.iter().enumerate() {
                    let sel = i == state.selected;
                    let resp = row(ui, it, sel);
                    if sel {
                        resp.scroll_to_me(None);
                    }
                    if resp.clicked() {
                        chosen = Some(i);
                    }
                }
                if scored.is_empty() {
                    ui.weak(tr("No matches"));
                }
            });
        });
    });
    // A click anywhere else closes the palette.
    if ctx.input(|i| i.pointer.any_pressed() && i.pointer.interact_pos().is_some_and(|p| !area.response.rect.contains(p))) {
        *open = false;
    }
    if let Some(i) = chosen {
        if let Some((_, it)) = scored.get(i) {
            match &it.target {
                Target::Action(a) => cx.push(AppCommand::Action(*a)),
                Target::Script(p) => cx.push(AppCommand::OpenScript(p.clone())),
                Target::Bookmark(c, o) => {
                    if !cx.conns.is_connected(c) {
                        cx.push(AppCommand::Connect(c.clone()));
                    }
                    cx.push(AppCommand::OpenObject { connection: c.clone(), object: o.clone(), page: ObjectPage::Properties });
                }
                Target::Command(f) => cx.push(f()),
                Target::Connect(c) => cx.push(AppCommand::Connect(c.clone())),
                Target::Editor(c) => {
                    cx.push(AppCommand::Connect(c.clone()));
                    cx.push(AppCommand::NewSqlEditor { connection: Some(c.clone()), schema: None, text: None });
                }
                Target::Object(c, o) => cx.push(AppCommand::OpenObject { connection: c.clone(), object: o.clone(), page: ObjectPage::Properties }),
            }
        }
        *open = false;
    }
    if *open {
        ctx.data_mut(|d| d.insert_temp(id, state));
    } else {
        ctx.data_mut(|d| d.remove::<PaletteState>(id));
    }
}

#[cfg(test)]
mod tests {
    use super::fuzzy;

    #[test]
    fn fuzzy_ranks_prefix_higher() {
        let a = fuzzy("ne", "New connection").unwrap();
        let b = fuzzy("ne", "Open SQL editor: Neon").unwrap();
        assert!(a > b);
        assert!(fuzzy("xyz", "New connection").is_none());
        // A word typed in one piece beats letters spread over several words.
        assert!(fuzzy("set", "Settings Application ⌘,").unwrap() > fuzzy("set", "Session Manager Tools").unwrap());
        assert!(fuzzy("export", "Export data Results").unwrap() > fuzzy("export", "Open recent export folder").unwrap());
    }
}
