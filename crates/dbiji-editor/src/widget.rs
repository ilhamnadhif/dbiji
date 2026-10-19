//! The [`SqlEditor`] widget: layout of gutter / text / find bar, pointer
//! interaction, painting and popups.

use std::ops::Range;
use std::sync::Arc;

use dbiji_core::tr;
use egui::epaint::text::CharIndex;
use egui::{
    Color32, Context, CursorIcon, EventFilter, FontId, Galley, Painter, PointerButton, Pos2, Rect,
    Response, ScrollArea, Sense, Shape, Stroke, Ui, Vec2, pos2, vec2,
};

use crate::completion;
use crate::find_bar;
use crate::input;
use crate::language::{CompletionEntry, EditorLanguage};
use crate::layout::{HighlightPolicy, LayoutParams};
use crate::ops::matching_bracket;
use crate::options::EditorOptions;
use crate::state::{DragMode, EditorState, ScrollRequest, Severity};
use crate::text::{Selection, word_range_at};
use crate::theme::EditorTheme;

/// Left padding between gutter and text.
const PAD_LEFT: f32 = 6.0;
/// Padding above the first line.
const PAD_TOP: f32 = 2.0;
/// Width reserved for gutter markers.
const MARK_W: f32 = 12.0;
/// Width of the fold marker column of the gutter.
const FOLD_W: f32 = 12.0;
/// Bracket matching scan limit (bytes in each direction).
const BRACKET_SCAN: usize = 64 * 1024;

/// An action offered by the context menu (and accepted by
/// [`EditorState::apply_action`]).
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContextAction {
    /// Execute the statement at the cursor (application).
    Execute,
    /// Execute the whole script (application).
    ExecuteScript,
    /// Execute in a new result tab (application).
    ExecuteNewTab,
    /// Explain the statement (application).
    Explain,
    /// Format the SQL (application; apply the result with
    /// [`EditorState::set_text`] or [`EditorState::replace_range`]).
    Format,
    /// Cut the selection (or line).
    Cut,
    /// Copy the selection (or line).
    Copy,
    /// Paste from the clipboard.
    Paste,
    /// Select everything.
    SelectAll,
    /// Undo.
    Undo,
    /// Redo.
    Redo,
    /// Toggle line comments.
    ToggleComment,
    /// Toggle a block comment.
    ToggleBlockComment,
    /// Upper-case the selection / word.
    UpperCase,
    /// Lower-case the selection / word.
    LowerCase,
    /// Open the find bar.
    Find,
    /// Open the find bar with replace.
    Replace,
    /// Open the go-to-line popup.
    GotoLine,
    /// Add a word to the spelling dictionary (application).
    AddSpellingWord(String),
    /// Replace a (misspelled) word with a suggestion.
    ReplaceWord {
        /// Byte range of the word.
        range: Range<usize>,
        /// Replacement.
        with: String,
    },
}

impl ContextAction {
    /// The application actions shown by default at the top of the context menu.
    pub const DEFAULT_APP_ACTIONS: &'static [ContextAction] = &[
        ContextAction::Execute,
        ContextAction::ExecuteScript,
        ContextAction::ExecuteNewTab,
        ContextAction::Explain,
        ContextAction::Format,
    ];

    /// Does [`EditorState::apply_action`] implement this action itself?
    /// (`false` for execute / explain / format / add-word.)
    pub fn is_editor_action(&self) -> bool {
        !matches!(
            self,
            ContextAction::Execute
                | ContextAction::ExecuteScript
                | ContextAction::ExecuteNewTab
                | ContextAction::Explain
                | ContextAction::Format
                | ContextAction::AddSpellingWord(_)
        )
    }

    /// Does the action modify the text?
    pub fn is_mutating(&self) -> bool {
        matches!(
            self,
            ContextAction::Cut
                | ContextAction::Paste
                | ContextAction::Undo
                | ContextAction::Redo
                | ContextAction::ToggleComment
                | ContextAction::ToggleBlockComment
                | ContextAction::UpperCase
                | ContextAction::LowerCase
                | ContextAction::ReplaceWord { .. }
                | ContextAction::Format
        )
    }

    /// Localised menu label.
    pub fn label(&self) -> String {
        match self {
            ContextAction::Execute => tr("Execute statement").to_owned(),
            ContextAction::ExecuteScript => tr("Execute script").to_owned(),
            ContextAction::ExecuteNewTab => tr("Execute in new tab").to_owned(),
            ContextAction::Explain => tr("Explain execution plan").to_owned(),
            ContextAction::Format => tr("Format SQL").to_owned(),
            ContextAction::Cut => tr("Cut").to_owned(),
            ContextAction::Copy => tr("Copy").to_owned(),
            ContextAction::Paste => tr("Paste").to_owned(),
            ContextAction::SelectAll => tr("Select all").to_owned(),
            ContextAction::Undo => tr("Undo").to_owned(),
            ContextAction::Redo => tr("Redo").to_owned(),
            ContextAction::ToggleComment => tr("Toggle comment").to_owned(),
            ContextAction::ToggleBlockComment => tr("Toggle block comment").to_owned(),
            ContextAction::UpperCase => tr("Upper case").to_owned(),
            ContextAction::LowerCase => tr("Lower case").to_owned(),
            ContextAction::Find => tr("Find").to_owned(),
            ContextAction::Replace => tr("Replace").to_owned(),
            ContextAction::GotoLine => tr("Go to line").to_owned(),
            ContextAction::AddSpellingWord(w) => dbiji_core::trf("Add \"{}\" to dictionary", &[w]),
            ContextAction::ReplaceWord { with, .. } => with.clone(),
        }
    }

    /// Phosphor icon for the menu entry.
    pub fn icon(&self) -> &'static str {
        use egui_phosphor::regular as ph;
        match self {
            ContextAction::Execute => ph::PLAY,
            ContextAction::ExecuteScript => ph::FAST_FORWARD,
            ContextAction::ExecuteNewTab => ph::TABS,
            ContextAction::Explain => ph::TREE_STRUCTURE,
            ContextAction::Format => ph::TEXT_ALIGN_LEFT,
            ContextAction::Cut => ph::SCISSORS,
            ContextAction::Copy => ph::COPY,
            ContextAction::Paste => ph::CLIPBOARD,
            ContextAction::SelectAll => ph::SELECTION_ALL,
            ContextAction::Undo => ph::ARROW_U_UP_LEFT,
            ContextAction::Redo => ph::ARROW_U_UP_RIGHT,
            ContextAction::ToggleComment | ContextAction::ToggleBlockComment => ph::HASH,
            ContextAction::UpperCase | ContextAction::LowerCase => ph::TEXT_AA,
            ContextAction::Find | ContextAction::Replace => ph::MAGNIFYING_GLASS,
            ContextAction::GotoLine => ph::LIST_NUMBERS,
            ContextAction::AddSpellingWord(_) => ph::BOOK_OPEN,
            ContextAction::ReplaceWord { .. } => ph::CHECK,
        }
    }
}

/// Something that happened in the editor this frame.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EditorEvent {
    /// The cursor or selection changed; the payload is the cursor offset.
    CursorMoved(usize),
    /// The text area gained (`true`) or lost keyboard focus.
    FocusChanged(bool),
    /// Command-click on a range returned by [`EditorLanguage::link_at`].
    LinkClicked {
        /// Byte range of the link.
        range: Range<usize>,
        /// Text of the link.
        text: String,
    },
    /// A completion entry was inserted.
    CompletionAccepted(CompletionEntry),
    /// A context-menu entry was chosen. Editor actions
    /// ([`ContextAction::is_editor_action`]) have already been applied.
    ContextMenu(ContextAction),
    /// The save shortcut was pressed while the editor had focus.
    SaveRequested,
}

/// What [`SqlEditor::show`] returns.
pub struct EditorResponse {
    /// Response of the text area.
    pub response: Response,
    /// The text changed this frame.
    pub changed: bool,
    /// Events of this frame, in order.
    pub events: Vec<EditorEvent>,
}

/// The SQL editor widget. Construct it every frame:
///
/// ```no_run
/// # fn ui(ui: &mut egui::Ui, state: &mut dbiji_editor::EditorState) {
/// use dbiji_editor::{BasicSqlLanguage, EditorOptions, EditorTheme, SqlEditor};
/// let lang = BasicSqlLanguage::new();
/// let options = EditorOptions::default();
/// let theme = EditorTheme::from_visuals(ui.visuals());
/// let out = SqlEditor::new(state, &lang, &options, &theme).show(ui);
/// for event in out.events {
///     // handle EditorEvent::ContextMenu(ContextAction::Execute), LinkClicked, ...
/// }
/// # }
/// ```
pub struct SqlEditor<'a> {
    state: &'a mut EditorState,
    lang: &'a dyn EditorLanguage,
    options: &'a EditorOptions,
    theme: &'a EditorTheme,
    app_actions: &'a [ContextAction],
}

/// Per-frame geometry shared by pointer handling and painting.
struct Frame {
    origin: Pos2,
    clip: Rect,
    gutter: Rect,
    text_left: f32,
    row_h: f32,
    text_dy: f32,
    char_w: f32,
    font_id: FontId,
}

impl<'a> SqlEditor<'a> {
    /// New editor widget for `state`.
    pub fn new(
        state: &'a mut EditorState,
        lang: &'a dyn EditorLanguage,
        options: &'a EditorOptions,
        theme: &'a EditorTheme,
    ) -> Self {
        Self {
            state,
            lang,
            options,
            theme,
            app_actions: ContextAction::DEFAULT_APP_ACTIONS,
        }
    }

    /// Application actions listed at the top of the context menu (default:
    /// [`ContextAction::DEFAULT_APP_ACTIONS`]; pass `&[]` to hide them).
    pub fn app_actions(mut self, actions: &'a [ContextAction]) -> Self {
        self.app_actions = actions;
        self
    }

    /// Show the editor filling the available space.
    pub fn show(self, ui: &mut Ui) -> EditorResponse {
        let SqlEditor {
            state: st,
            lang,
            options,
            theme,
            app_actions,
        } = self;
        let ctx = ui.ctx().clone();
        st.now = ui.input(|i| i.time);
        st.edited_this_frame = false;
        let start_version = st.version();
        let mut events = Vec::new();
        st.undo.set_limit(options.undo_limit);

        // Metrics.
        let font_id = FontId::new(options.font_size.max(4.0), options.font_family.clone());
        let ppp = ctx.pixels_per_point();
        let (font_h, char_w) =
            ctx.fonts_mut(|f| (f.row_height(&font_id), f.glyph_width(&font_id, '0')));
        let char_w = if char_w > 0.0 {
            char_w
        } else {
            font_id.size * 0.6
        };
        let row_h = ((font_h + options.line_spacing.max(0.0)) * ppp).round() / ppp;
        let text_dy = (((row_h - font_h) / 2.0).max(0.0) * ppp).floor() / ppp;
        let digits = st.line_count().to_string().len().max(2) as f32;
        let gutter_w = if options.line_numbers {
            (MARK_W + digits * char_w + 10.0).round()
        } else {
            MARK_W
        } + if options.folding { FOLD_W } else { 0.0 };

        if st.find.open {
            find_bar::show_find_bar(ui, st, lang, options, theme);
        }

        let mut avail = ui.available_size_before_wrap();
        if !avail.y.is_finite() {
            // Inside an unbounded parent (e.g. a vertical ScrollArea): use a fixed height.
            avail.y = row_h * 20.0;
            ui.set_max_height(avail.y);
        }
        let scroll_w = ui.spacing().scroll.allocated_width();
        let wrap = options.word_wrap.then(|| {
            (avail.x - gutter_w - PAD_LEFT - scroll_w - char_w)
                .max(char_w * 12.0)
                .floor()
        });
        let params = LayoutParams {
            font_id: font_id.clone(),
            row_height: row_h,
            char_width: char_w,
            wrap_width: wrap,
            pixels_per_point: ppp,
        };
        // Keep the first visible line in place when row geometry changes
        // (re-wrapping after a resize, font change, background refinement of
        // estimated rows).
        let anchor_y = st.scroll_offset.y - PAD_TOP;
        let anchor_line = st.layout.line_at_y(anchor_y);
        let rows_into_anchor =
            (anchor_y - st.layout.line_top(anchor_line)) / st.layout.row_height();
        if options.folding {
            st.sync_folds(lang);
            if st.folds.is_stale(st.version()) {
                ctx.request_repaint_after(std::time::Duration::from_millis(450));
            }
        } else {
            st.clear_folds();
        }
        st.layout
            .set_params(&ctx, &st.text, &st.index, params, theme);
        if st.layout.has_estimates() {
            let rows = (st.viewport_size.y / row_h).ceil() as usize + 2;
            if st
                .layout
                .refine(&ctx, &st.text, &st.index, anchor_line..anchor_line + rows)
            {
                ctx.request_repaint();
            }
        }
        if anchor_y > 0.0 && anchor_line < st.line_count() && st.restore_scroll.is_none() {
            let line_h = st.layout.line_rows(anchor_line) as f32 * row_h;
            let within = (rows_into_anchor * row_h).clamp(0.0, (line_h - 1.0).max(0.0));
            let target = PAD_TOP + st.layout.line_top(anchor_line) + within;
            if (target - st.scroll_offset.y).abs() > 0.5 {
                st.scroll_offset.y = target;
                st.restore_scroll = Some(st.scroll_offset);
            }
        }

        // Keyboard.
        let had_focus = ctx.memory(|m| m.has_focus(st.id));
        let mut typed = Vec::new();
        if had_focus {
            typed = input::handle_keyboard(st, &ctx, lang, options, &mut events).typed;
            if options.folding {
                // A cursor moved into a collapsed region opens it.
                st.sync_folds(lang);
            }
        }

        // Highlighting.
        let policy = HighlightPolicy {
            enabled: st.text.len() <= options.max_highlight_bytes,
            sync_limit: options.sync_highlight_bytes,
            debounce: options.highlight_debounce_ms as f64 / 1000.0,
        };
        if let Some(wait) = st.layout.update_highlight(
            &st.text,
            &st.index,
            lang,
            policy,
            st.now,
            st.edited_this_frame,
        ) {
            ctx.request_repaint_after_secs(wait as f32 + 0.001);
        }

        completion::update(st, &ctx, lang, options, &typed);
        find_bar::refresh(st, lang);
        if let Some(due) = st.pending_find_rescan {
            ctx.request_repaint_after_secs(((due - st.now).max(0.0) + 0.001) as f32);
        }

        // Scrolling.
        let mut area = if options.word_wrap {
            ScrollArea::vertical()
        } else {
            ScrollArea::both()
        }
        .id_salt(st.id.with("scroll"))
        .auto_shrink([false, false])
        .content_margin(0.0)
        .animated(false);
        if let Some(off) = st.restore_scroll.take() {
            st.scroll_offset = off;
            area = area.scroll_offset(off);
        }
        if let Some(req) = st.pending_scroll.take() {
            let off = scroll_target(st, &ctx, req, gutter_w, row_h, char_w);
            if off != st.scroll_offset {
                st.scroll_offset = off;
                area = area.scroll_offset(off);
            }
        }

        let mut caret_screen = None;
        let output = area.show_viewport(ui, |ui, viewport| {
            let total_h = st.layout.total_height();
            let max_w = st.layout.max_width();
            let size = vec2(
                (gutter_w + PAD_LEFT + max_w + char_w * 4.0).max(viewport.width()),
                (PAD_TOP + total_h + row_h).max(viewport.height()),
            );
            let (rect, _) = ui.allocate_exact_size(size, Sense::hover());
            let clip = ui.clip_rect();
            let frame = Frame {
                origin: rect.min + vec2(gutter_w + PAD_LEFT, PAD_TOP),
                clip,
                gutter: Rect::from_min_max(
                    pos2(clip.left(), clip.top()),
                    pos2(clip.left() + gutter_w, clip.bottom()),
                ),
                text_left: clip.left() + gutter_w,
                row_h,
                text_dy,
                char_w,
                font_id: font_id.clone(),
            };
            let response = ui.interact(rect, st.id, Sense::click_and_drag());
            let gutter_resp =
                ui.interact(frame.gutter, st.id.with("gutter"), Sense::click_and_drag());
            if response.has_focus() {
                ui.memory_mut(|m| {
                    m.set_focus_lock_filter(
                        st.id,
                        EventFilter {
                            tab: true,
                            horizontal_arrows: true,
                            vertical_arrows: true,
                            escape: true,
                        },
                    )
                });
            }
            if std::mem::take(&mut st.pending_focus) {
                response.request_focus();
            }
            pointer(
                ui,
                st,
                lang,
                options,
                &frame,
                &response,
                &gutter_resp,
                &mut events,
            );

            let vis = (viewport.min.y - PAD_TOP)..(viewport.max.y - PAD_TOP);
            let lines = st.layout.lines_in(vis);
            st.occurrences = if options.mark_occurrences && st.sel.is_empty() && !lines.is_empty() {
                occurrences(st, lang, &lines)
            } else {
                Vec::new()
            };
            st.layout
                .ensure_galleys(&ctx, &st.text, &st.index, lines.clone());
            let focused = response.has_focus();
            caret_screen = paint(ui, st, options, theme, &frame, lines.clone(), focused);
            accessibility(ui, st, &frame, lines.clone(), focused);
            st.layout.evict(lines);
            hover_tooltips(ui, st, lang, &frame, &response, &gutter_resp);
            if focused
                && !options.read_only
                && let Some(caret) = caret_screen
            {
                let to_global = ctx
                    .layer_transform_to_global(ui.layer_id())
                    .unwrap_or_default();
                // egui-winit places the candidate window at `rect` (not at
                // `cursor_rect`), so report the caret for both: in a large
                // editor the text area's corner can be far from the caret.
                let caret = if clip.intersects(caret) {
                    caret
                } else {
                    Rect::from_min_size(clip.left_top(), caret.size())
                };
                let caret_global = to_global * caret;
                ctx.output_mut(|o| {
                    o.ime = Some(egui::output::IMEOutput {
                        purpose: egui::IMEPurpose::Normal,
                        rect: caret_global,
                        cursor_rect: caret_global,
                        should_interrupt_composition: false,
                    });
                });
            }
            response
        });
        st.scroll_offset = output.state.offset;
        st.viewport_size = output.inner_rect.size();
        let mut response = output.inner;

        // Popups.
        if st.completion.is_open()
            && let Some(caret) = caret_screen
        {
            let anchor_x = caret.left();
            let anchor = Rect::from_min_max(
                pos2(anchor_x, caret.top()),
                pos2(anchor_x + 1.0, caret.bottom()),
            );
            let (clicked, interacted) = completion::show_popup(
                &ctx,
                st.id.with("completion"),
                anchor,
                st,
                theme,
                options.font_size,
            );
            if let Some(i) = clicked {
                st.completion.select(i);
                if let Some(entry) = completion::accept(st) {
                    events.push(EditorEvent::CompletionAccepted(entry));
                }
            }
            if interacted || clicked.is_some() {
                ctx.memory_mut(|m| m.request_focus(st.id));
            }
        }
        find_bar::show_goto(&ctx, response.layer_id, st, output.inner_rect, theme);
        if options.context_menu {
            context_menu(st, &ctx, lang, options, &response, app_actions, &mut events);
        }

        // Accessibility / output events. The value is only built when egui
        // asks for it (focus, click or change events); very large documents
        // report the cursor line only.
        let enabled = ui.is_enabled();
        if st.version() != start_version {
            response.mark_changed();
        }
        {
            let text = st.text.as_str();
            let cursor_line = st.index.line_range(st.index.line_of_offset(st.sel.cursor));
            let value = || {
                if text.len() <= 100_000 {
                    text
                } else {
                    &text[cursor_line.clone()]
                }
            };
            response.widget_info(|| egui::WidgetInfo::text_edit(enabled, value(), value(), ""));
        }
        ctx.accesskit_node_builder(st.id, |b| {
            b.set_role(egui::accesskit::Role::MultilineTextInput);
            b.set_value(st.text.clone());
            if options.read_only {
                b.set_read_only();
            }
        });

        // Events.
        let focused = ctx.memory(|m| m.has_focus(st.id));
        if focused != st.had_focus {
            if !focused && st.preedit.is_some() {
                st.preedit = None;
                st.undo.close_group();
            }
            if !focused {
                st.completion.close();
            }
            st.had_focus = focused;
            events.push(EditorEvent::FocusChanged(focused));
        }
        if st.last_reported_cursor != Some(st.sel) {
            if st.last_reported_cursor.is_some() {
                events.push(EditorEvent::CursorMoved(st.sel.cursor));
            }
            st.last_reported_cursor = Some(st.sel);
        }
        EditorResponse {
            response,
            changed: st.version() != start_version,
            events,
        }
    }
}

/// Scroll offset that brings the cursor into view (or centers it).
fn scroll_target(
    st: &mut EditorState,
    ctx: &Context,
    req: ScrollRequest,
    gutter_w: f32,
    row_h: f32,
    char_w: f32,
) -> Vec2 {
    let c = st.sel.cursor;
    let line = st.index.line_of_offset(c);
    let col = st.index.char_col(&st.text, c);
    let caret = st
        .layout
        .caret_rect(ctx, &st.text, &st.index, line, col, st.prefer_next_row);
    let view = st.viewport_size;
    let mut off = st.scroll_offset;
    let top = PAD_TOP + caret.min.y;
    let bottom = top + row_h;
    match req {
        ScrollRequest::Center => {
            if top < off.y || bottom > off.y + view.y {
                off.y = (top - view.y * 0.4).max(0.0);
            }
        }
        ScrollRequest::Cursor => {
            if top < off.y {
                off.y = top - PAD_TOP;
            } else if bottom > off.y + view.y {
                off.y = bottom - view.y + row_h * 0.5;
            }
        }
    }
    let text_w = (view.x - gutter_w - PAD_LEFT).max(char_w * 4.0);
    let x = caret.min.x;
    if x < off.x {
        off.x = (x - char_w * 4.0).max(0.0);
    } else if x > off.x + text_w - char_w {
        off.x = x - text_w + char_w * 4.0;
    }
    off.max(Vec2::ZERO)
}

/// Byte offset at screen position `p`.
fn offset_at(ui: &Ui, st: &mut EditorState, frame: &Frame, p: Pos2) -> (usize, bool) {
    let ctx = ui.ctx().clone();
    let doc = p - frame.origin;
    let (line, col, aff) = st.layout.hit(&ctx, &st.text, &st.index, doc);
    (st.index.offset_of_char_col(&st.text, line, col), aff)
}

#[allow(clippy::too_many_arguments)]
fn pointer(
    ui: &mut Ui,
    st: &mut EditorState,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    frame: &Frame,
    response: &Response,
    gutter: &Response,
    events: &mut Vec<crate::widget::EditorEvent>,
) {
    let (pos, primary_pressed, secondary_pressed, primary_down, modifiers) = ui.input(|i| {
        (
            i.pointer.interact_pos(),
            i.pointer.primary_pressed(),
            i.pointer.secondary_pressed(),
            i.pointer.primary_down(),
            i.modifiers,
        )
    });
    let is_word = |c: char| lang.is_word_char(c);
    if response.hovered() {
        ui.ctx().set_cursor_icon(CursorIcon::Text);
        ui.output_mut(|o| o.mutable_text_under_cursor = !options.read_only);
    }
    // Command + hover hyperlinks.
    let mut link: Option<Range<usize>> = None;
    if response.hovered()
        && modifiers.command
        && let Some(p) = response.hover_pos()
    {
        let (off, _) = offset_at(ui, st, frame, p);
        let version = st.version();
        link = match &st.link_cache {
            Some((v, o, r)) if *v == version && *o == off => r.clone(),
            _ => {
                let r = lang
                    .link_at(&st.text, off)
                    .map(|r| crate::text::clamp_range(&st.text, r))
                    .filter(|r| !r.is_empty());
                st.link_cache = Some((version, off, r.clone()));
                r
            }
        };
        if link.is_some() {
            ui.ctx().set_cursor_icon(CursorIcon::PointingHand);
        }
    }
    st.hover_link = link.clone();
    let Some(p) = pos else {
        if !primary_down {
            st.drag_select = None;
        }
        return;
    };
    if (primary_pressed || secondary_pressed)
        && (response.hovered() || gutter.hovered())
        && st.preedit.is_some()
    {
        // Clicking interrupts the composition (egui asks the platform to
        // end it); keep the pre-edit text as typed text.
        st.preedit = None;
        st.ime_cleared_at = None;
        st.undo.close_group();
    }

    // Gutter: fold markers.
    if options.folding
        && gutter.hovered()
        && primary_pressed
        && p.x >= frame.gutter.right() - FOLD_W - 3.0
    {
        let line = st.layout.line_at_y(p.y - frame.origin.y);
        if st.folds.toggle(line) {
            let cursor_line = st.index.line_of_offset(st.sel.cursor);
            if let Some((a, b)) = st.folds.region_at(line)
                && st.folds.collapsed.contains(&a)
                && cursor_line > a
                && cursor_line <= b
            {
                st.sel = crate::text::Selection::caret(st.index.line_start(a));
            }
            st.sync_folds(lang);
            ui.ctx().request_repaint();
            return;
        }
    }
    // Gutter: line selection.
    if gutter.hovered() && primary_pressed {
        gutter.request_focus();
        let line = st.layout.line_at_y(p.y - frame.origin.y);
        let anchor_line = if modifiers.shift {
            st.index.line_of_offset(st.sel.anchor)
        } else {
            line
        };
        select_lines(st, anchor_line, line);
        st.drag_select = Some(DragMode::Lines(anchor_line));
        st.undo.close_group();
        response.request_focus();
        return;
    }
    if let Some(DragMode::Lines(anchor_line)) = st.drag_select {
        if primary_down && (gutter.dragged() || response.dragged()) {
            let line = st.layout.line_at_y(p.y - frame.origin.y);
            select_lines(st, anchor_line, line);
        } else if !primary_down {
            st.drag_select = None;
        }
        return;
    }

    if response.hovered() && primary_pressed {
        response.request_focus();
        let (off, aff) = offset_at(ui, st, frame, p);
        if modifiers.shift {
            st.set_selection(Selection::new(st.sel.anchor, off));
        } else {
            st.set_selection(Selection::caret(off));
        }
        st.prefer_next_row = aff;
        st.h_pos = None;
        st.undo.close_group();
        st.drag_select = Some(DragMode::Chars);
    } else if response.double_clicked() && !modifiers.command {
        let (off, _) = offset_at(ui, st, frame, p);
        if let Some(r) = word_range_at(&st.text, off, is_word) {
            st.set_selection(Selection::new(r.start, r.end));
        }
    } else if response.triple_clicked() {
        let (off, _) = offset_at(ui, st, frame, p);
        let line = st.index.line_of_offset(off);
        select_lines(st, line, line);
    } else if primary_down
        && st.drag_select == Some(DragMode::Chars)
        && response.dragged_by(PointerButton::Primary)
    {
        let (off, aff) = offset_at(ui, st, frame, p);
        st.set_selection(Selection::new(st.sel.anchor, off));
        st.prefer_next_row = aff;
        // Auto-scroll when dragging outside the viewport.
        let mut delta = Vec2::ZERO;
        if p.y < frame.clip.top() {
            delta.y = (frame.clip.top() - p.y).min(frame.row_h * 3.0);
        } else if p.y > frame.clip.bottom() {
            delta.y = -(p.y - frame.clip.bottom()).min(frame.row_h * 3.0);
        }
        if p.x < frame.text_left {
            delta.x = (frame.text_left - p.x).min(frame.char_w * 4.0);
        } else if p.x > frame.clip.right() {
            delta.x = -(p.x - frame.clip.right()).min(frame.char_w * 4.0);
        }
        if delta != Vec2::ZERO {
            ui.scroll_with_delta(delta);
            ui.ctx().request_repaint();
        }
    }
    if !primary_down && st.drag_select.is_some() {
        st.drag_select = None;
    }

    if response.clicked()
        && modifiers.command
        && let Some(r) = link
    {
        events.push(EditorEvent::LinkClicked {
            text: st.text[r.clone()].to_owned(),
            range: r,
        });
    }

    // Secondary press: keep the selection if inside it, else move the caret.
    if response.hovered() && secondary_pressed {
        response.request_focus();
        let (off, _) = offset_at(ui, st, frame, p);
        let r = st.sel.range();
        if st.sel.is_empty() || off < r.start || off > r.end {
            st.set_selection(Selection::caret(off));
            st.undo.close_group();
        }
        st.context_offset = Some(off);
        st.spell_menu = st
            .spell
            .iter()
            .find(|s| s.start <= off && off <= s.end)
            .map(|s| {
                let word = st.text[s.clone()].to_owned();
                let suggestions = lang.spell_suggestions(&word);
                (s.clone(), word, suggestions)
            });
    }
}

/// Expose the visible lines as AccessKit text runs (with the caret /
/// selection) when a screen reader is active. The whole document is the
/// value of the text node; runs cover what is on screen, matching what a
/// virtualized view can describe without laying out every line.
fn accessibility(ui: &Ui, st: &mut EditorState, frame: &Frame, lines: Range<usize>, focused: bool) {
    let ctx = ui.ctx().clone();
    if lines.is_empty() || ctx.accesskit_node_builder(st.id, |_| ()).is_none() {
        st.a11y_view = None;
        return;
    }
    let start = st.index.line_start(lines.start);
    let end = st.index.line_end(lines.end - 1);
    let galleys: Vec<Arc<Galley>> = lines
        .clone()
        .map(|l| st.layout.galley(&ctx, &st.text, &st.index, l))
        .collect();
    let job = egui::text::LayoutJob::single_section(
        st.text[start..end].to_owned(),
        egui::text::TextFormat::simple(frame.font_id.clone(), Color32::PLACEHOLDER),
    );
    let galley = Arc::new(Galley::concat(
        Arc::new(job),
        &galleys,
        ctx.pixels_per_point(),
    ));
    let base = st.index.byte_to_char(&st.text, start);
    let visible_chars = st.index.byte_to_char(&st.text, end) - base;
    let to_local = |o: usize| {
        egui::text::CCursor::new(
            st.index
                .byte_to_char(&st.text, o)
                .saturating_sub(base)
                .min(visible_chars),
        )
    };
    let range = focused
        .then(|| egui::text::CCursorRange::two(to_local(st.sel.anchor), to_local(st.sel.cursor)));
    let top = st.layout.line_top(lines.start);
    let origin = egui::emath::TSTransform::from_translation(vec2(
        frame.origin.x,
        frame.origin.y + top + frame.text_dy,
    ));
    egui::text_selection::accesskit_text::update_accesskit_for_text_widget(
        &ctx,
        st.id,
        range,
        egui::accesskit::Role::MultilineTextInput,
        origin,
        &galley,
    );
    st.a11y_view = Some((galley, base));
}

fn select_lines(st: &mut EditorState, a: usize, b: usize) {
    let (lo, hi) = (a.min(b), a.max(b));
    let start = st.index.line_start(lo);
    let end = st.index.line_range_with_newline(hi).end;
    if b >= a {
        st.set_selection(Selection::new(start, end));
    } else {
        st.set_selection(Selection::new(
            st.index.line_range_with_newline(a).end,
            st.index.line_start(b),
        ));
    }
}

/// Screen rectangles covering char columns `cols` of `line` (one per row).
fn range_rects(
    st: &mut EditorState,
    ctx: &Context,
    frame: &Frame,
    line: usize,
    cols: Range<usize>,
    extend_newline: bool,
    min_width: f32,
) -> Vec<Rect> {
    let g: Arc<Galley> = st.layout.galley(ctx, &st.text, &st.index, line);
    let top = st.layout.line_top(line);
    let mut out = Vec::new();
    let mut row_start = 0usize;
    let n_rows = g.rows.len();
    for (ri, row) in g.rows.iter().enumerate() {
        let n = row.glyphs.len();
        let row_end = row_start + n;
        let last = ri + 1 == n_rows;
        let s = cols.start.max(row_start);
        let e = cols.end.min(row_end);
        let touches = s < e
            || (cols.start == cols.end
                && cols.start >= row_start
                && (cols.start < row_end || last));
        if touches || (extend_newline && last && cols.end >= row_end && cols.start <= row_end) {
            let x0 = row.pos.x + row.x_offset(CharIndex(s.saturating_sub(row_start).min(n)));
            let mut x1 = row.pos.x + row.x_offset(CharIndex(e.saturating_sub(row_start).min(n)));
            if extend_newline && last && cols.end >= row_end {
                x1 += frame.char_w * 0.6;
            }
            if x1 - x0 < min_width {
                x1 = x0 + min_width;
            }
            let y = frame.origin.y + top + row.pos.y;
            out.push(Rect::from_min_max(
                pos2(frame.origin.x + x0, y),
                pos2(frame.origin.x + x1, y + frame.row_h),
            ));
        }
        row_start = row_end;
    }
    out
}

/// Rectangles of a byte range intersecting the visible `lines`.
fn byte_range_rects(
    st: &mut EditorState,
    ctx: &Context,
    frame: &Frame,
    range: Range<usize>,
    lines: &Range<usize>,
    min_width: f32,
) -> Vec<Rect> {
    let mut out = Vec::new();
    if lines.is_empty() {
        return out;
    }
    let first = st.index.line_of_offset(range.start).max(lines.start);
    let last = st.index.line_of_offset(range.end).min(lines.end - 1);
    for l in first..=last {
        if st.layout.is_hidden(l) {
            continue;
        }
        let lr = st.index.line_range(l);
        if range.end < lr.start || range.start > lr.end {
            continue;
        }
        let s = range.start.max(lr.start);
        let e = range.end.min(lr.end);
        let cs = st.index.char_col(&st.text, s);
        let ce = st.index.char_col(&st.text, e);
        let extend = range.end > lr.end;
        out.extend(range_rects(st, ctx, frame, l, cs..ce, extend, min_width));
    }
    out
}

fn squiggle(painter: &Painter, rect: Rect, color: Color32) {
    let y = rect.bottom() - 1.5;
    let amp = 1.5;
    let step = 2.0;
    let mut pts = Vec::new();
    let mut x = rect.left();
    let mut up = true;
    while x <= rect.right() {
        pts.push(pos2(x, if up { y - amp } else { y + amp }));
        up = !up;
        x += step;
    }
    if pts.len() >= 2 {
        painter.add(Shape::line(pts, Stroke::new(1.0, color)));
    }
}

#[allow(clippy::too_many_arguments)]
/// Occurrences of the word under the cursor in the visible `lines`
/// (case-insensitive, whole words; none when it occurs only once).
fn occurrences(
    st: &EditorState,
    lang: &dyn EditorLanguage,
    lines: &Range<usize>,
) -> Vec<Range<usize>> {
    let Some(word) = st.word_at(lang, st.sel.cursor) else {
        return Vec::new();
    };
    let w = &st.text[word.clone()];
    if w.chars().count() < 2 || w.chars().all(|c| c.is_ascii_digit()) {
        return Vec::new();
    }
    let start = st.index.line_start(lines.start);
    let end = st.index.line_end(lines.end - 1);
    let hay = &st.text[start..end];
    let lower_hay = hay.to_lowercase();
    let lower_w = w.to_lowercase();
    if lower_hay.len() != hay.len() || lower_w.len() != w.len() {
        return Vec::new(); // lowercase changed byte lengths: skip rather than misplace
    }
    let is_word = |c: Option<char>| c.is_some_and(|c| lang.is_word_char(c));
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(i) = lower_hay[from..].find(&lower_w) {
        let a = from + i;
        let b = a + lower_w.len();
        if !is_word(hay[..a].chars().next_back()) && !is_word(hay[b..].chars().next()) {
            out.push(start + a..start + b);
            if out.len() >= 500 {
                break;
            }
        }
        from = b;
    }
    if out.len() < 2 { Vec::new() } else { out }
}

fn paint(
    ui: &Ui,
    st: &mut EditorState,
    options: &EditorOptions,
    theme: &EditorTheme,
    frame: &Frame,
    lines: Range<usize>,
    focused: bool,
) -> Option<Rect> {
    let ctx = ui.ctx().clone();
    let painter = ui.painter().with_clip_rect(frame.clip);
    let text_clip = Rect::from_min_max(pos2(frame.text_left, frame.clip.top()), frame.clip.max);
    let tp = painter.with_clip_rect(text_clip);
    painter.rect_filled(frame.clip, 0.0, theme.background);
    if lines.is_empty() {
        return None;
    }
    let vis_start = st.index.line_start(lines.start);
    let vis_end = st.index.line_end(lines.end - 1);
    let band = |top: f32, rows: f32| {
        Rect::from_min_max(
            pos2(frame.text_left, frame.origin.y + top),
            pos2(
                frame.clip.right(),
                frame.origin.y + top + rows * frame.row_h,
            ),
        )
    };

    // Statement and current line backgrounds.
    if let Some(r) = st.statement.clone() {
        let a = st.index.line_of_offset(r.start).max(lines.start);
        let b = st.index.line_of_offset(r.end).min(lines.end - 1);
        if a <= b {
            let top = st.layout.line_top(a);
            let bottom = st.layout.line_top(b) + st.layout.line_rows(b) as f32 * frame.row_h;
            tp.rect_filled(
                band(top, (bottom - top) / frame.row_h),
                0.0,
                theme.statement_background,
            );
        }
    }
    let cursor_line = st.index.line_of_offset(st.sel.cursor);
    if options.highlight_current_line
        && st.sel.is_empty()
        && lines.contains(&cursor_line)
        && !st.layout.is_hidden(cursor_line)
    {
        let top = st.layout.line_top(cursor_line);
        tp.rect_filled(
            band(top, st.layout.line_rows(cursor_line) as f32),
            0.0,
            theme.current_line,
        );
    }

    // Find matches.
    let matches = st.find.visible_matches();
    if !matches.is_empty() {
        let i0 = matches.partition_point(|m| m.end < vis_start);
        let visible: Vec<(Range<usize>, bool)> = matches[i0..]
            .iter()
            .enumerate()
            .take_while(|(_, m)| m.start <= vis_end)
            .take(5000)
            .map(|(k, m)| (m.clone(), st.find.current == Some(i0 + k)))
            .collect();
        for (m, current) in visible {
            let color = if current {
                theme.find_current
            } else {
                theme.find_match
            };
            for r in byte_range_rects(st, &ctx, frame, m, &lines, 2.0) {
                tp.rect_filled(r, 2.0, color);
            }
        }
    }

    // Occurrences of the word under the cursor.
    for m in std::mem::take(&mut st.occurrences) {
        for r in byte_range_rects(st, &ctx, frame, m, &lines, 0.0) {
            tp.rect_filled(r, 2.0, theme.occurrence);
        }
    }

    // Selection.
    if !st.sel.is_empty() {
        let color = if focused {
            theme.selection
        } else {
            theme.selection_inactive
        };
        let r = st.sel.range();
        if r.end >= vis_start && r.start <= vis_end + 1 {
            for rect in byte_range_rects(st, &ctx, frame, r, &lines, 0.0) {
                tp.rect_filled(rect, 0.0, color);
            }
        }
    }

    // Matching brackets.
    if options.highlight_matching_brackets && focused && st.sel.is_empty() {
        let layout = &st.layout;
        let index = &st.index;
        let is_code = |o: usize| layout.style_at(index, o).is_none_or(|s| !s.is_literal());
        if let Some((a, b)) = matching_bracket(&st.text, st.sel.cursor, is_code, BRACKET_SCAN) {
            for o in [a, b] {
                if o >= vis_start && o <= vis_end {
                    let end = crate::text::next_char_boundary(&st.text, o);
                    for r in byte_range_rects(st, &ctx, frame, o..end, &lines, 2.0) {
                        tp.rect_filled(r, 2.0, theme.bracket_match);
                        tp.rect_stroke(
                            r,
                            2.0,
                            Stroke::new(1.0, theme.bracket_outline),
                            egui::StrokeKind::Inside,
                        );
                    }
                }
            }
        }
    }

    // Text.
    for l in lines.clone() {
        if st.layout.is_hidden(l) {
            continue;
        }
        let g = st.layout.galley(&ctx, &st.text, &st.index, l);
        let top = st.layout.line_top(l);
        let width = g.rect.width();
        tp.galley(
            pos2(frame.origin.x, frame.origin.y + top + frame.text_dy),
            g,
            theme.text,
        );
        if options.folding && st.folds.collapsed.contains(&l) {
            // Placeholder of the hidden lines.
            let x = frame.origin.x + width + frame.char_w;
            let r = Rect::from_min_size(
                pos2(x, frame.origin.y + top + 2.0),
                vec2(frame.char_w * 3.0, frame.row_h - 4.0),
            );
            tp.rect_stroke(
                r,
                3.0,
                Stroke::new(1.0, theme.gutter_text),
                egui::StrokeKind::Inside,
            );
            tp.text(
                r.center(),
                egui::Align2::CENTER_CENTER,
                "…",
                frame.font_id.clone(),
                theme.gutter_text,
            );
        }
    }

    // Diagnostics, spelling, links, IME.
    let diags: Vec<(Range<usize>, Severity)> = st
        .diagnostics
        .iter()
        .filter(|d| d.range.end >= vis_start && d.range.start <= vis_end)
        .take(2000)
        .map(|d| (d.range.clone(), d.severity))
        .collect();
    for (range, sev) in diags {
        let color = match sev {
            Severity::Error => theme.error,
            Severity::Warning => theme.warning,
            Severity::Info => theme.info,
        };
        for r in byte_range_rects(st, &ctx, frame, range, &lines, frame.char_w) {
            squiggle(&tp, r, color);
        }
    }
    let spell: Vec<Range<usize>> = st
        .spell
        .iter()
        .filter(|r| r.end >= vis_start && r.start <= vis_end)
        .take(2000)
        .cloned()
        .collect();
    for range in spell {
        for r in byte_range_rects(st, &ctx, frame, range, &lines, 0.0) {
            let y = r.bottom() - 1.0;
            tp.extend(Shape::dotted_line(
                &[pos2(r.left(), y), pos2(r.right(), y)],
                theme.spelling,
                2.5,
                0.8,
            ));
        }
    }
    if let Some(link) = st.hover_link.clone() {
        for r in byte_range_rects(st, &ctx, frame, link, &lines, 0.0) {
            tp.hline(r.x_range(), r.bottom() - 1.5, Stroke::new(1.0, theme.link));
        }
    }
    if let Some(p) = st.preedit.clone() {
        for r in byte_range_rects(st, &ctx, frame, p, &lines, 0.0) {
            tp.hline(
                r.x_range(),
                r.bottom() - 1.0,
                Stroke::new(1.5, theme.ime_underline),
            );
        }
    }

    // Caret.
    let col = st.index.char_col(&st.text, st.sel.cursor);
    let caret_doc = st.layout.caret_rect(
        &ctx,
        &st.text,
        &st.index,
        cursor_line,
        col,
        st.prefer_next_row,
    );
    let caret = Rect::from_min_size(
        frame.origin + caret_doc.min.to_vec2(),
        vec2(2.0, frame.row_h),
    );
    if focused && ui.input(|i| i.focused) && lines.contains(&cursor_line) {
        let visuals = &ui.visuals().text_cursor;
        let visible = if options.cursor_blink && visuals.blink {
            let (on, off) = (
                visuals.on_duration.max(0.05) as f64,
                visuals.off_duration.max(0.05) as f64,
            );
            let t = (st.now - st.blink_epoch).max(0.0) % (on + off);
            let wake = if t < on { on - t } else { on + off - t };
            ctx.request_repaint_after_secs(wake as f32);
            t < on
        } else {
            true
        };
        if visible {
            tp.rect_filled(caret, 0.0, theme.cursor);
        }
    }

    // Gutter.
    paint_gutter(&painter, st, options, theme, frame, lines, cursor_line);
    Some(caret)
}

fn paint_gutter(
    painter: &Painter,
    st: &mut EditorState,
    options: &EditorOptions,
    theme: &EditorTheme,
    frame: &Frame,
    lines: Range<usize>,
    cursor_line: usize,
) {
    let g = frame.gutter;
    painter.rect_filled(g, 0.0, theme.gutter_background);
    painter.vline(
        g.right() - 0.5,
        g.y_range(),
        Stroke::new(1.0, theme.gutter_separator),
    );
    // Statement bar.
    if let Some(r) = st.statement.clone() {
        let a = st.index.line_of_offset(r.start).max(lines.start);
        let b = st
            .index
            .line_of_offset(r.end)
            .min(lines.end.saturating_sub(1));
        if a <= b {
            let top = frame.origin.y + st.layout.line_top(a);
            let bottom = frame.origin.y
                + st.layout.line_top(b)
                + st.layout.line_rows(b) as f32 * frame.row_h;
            painter.rect_filled(
                Rect::from_min_max(pos2(g.right() - 3.0, top), pos2(g.right() - 1.0, bottom)),
                0.0,
                theme.statement_marker,
            );
        }
    }
    // Diagnostic markers (max severity per line).
    let mut marks: Vec<(usize, Severity)> = Vec::new();
    for d in &st.diagnostics {
        let line = st.index.line_of_offset(d.range.start);
        if !lines.contains(&line) {
            continue;
        }
        match marks.iter_mut().find(|(l, _)| *l == line) {
            Some((_, s)) => *s = (*s).max(d.severity),
            None => marks.push((line, d.severity)),
        }
    }
    for (line, sev) in marks {
        let color = match sev {
            Severity::Error => theme.error,
            Severity::Warning => theme.warning,
            Severity::Info => theme.info,
        };
        let y = frame.origin.y + st.layout.line_top(line) + frame.row_h / 2.0;
        painter.circle_filled(pos2(g.left() + MARK_W / 2.0 + 1.0, y), 3.5, color);
    }
    let fold_w = if options.folding { FOLD_W } else { 0.0 };
    if options.folding {
        for l in lines.clone() {
            if st.layout.is_hidden(l) || st.folds.region_at(l).is_none() {
                continue;
            }
            let collapsed = st.folds.collapsed.contains(&l);
            let c = pos2(
                g.right() - 3.0 - FOLD_W / 2.0,
                frame.origin.y + st.layout.line_top(l) + frame.row_h / 2.0,
            );
            let s = 3.5;
            let pts = if collapsed {
                vec![
                    pos2(c.x - s * 0.6, c.y - s),
                    pos2(c.x + s * 0.8, c.y),
                    pos2(c.x - s * 0.6, c.y + s),
                ]
            } else {
                vec![
                    pos2(c.x - s, c.y - s * 0.6),
                    pos2(c.x + s, c.y - s * 0.6),
                    pos2(c.x, c.y + s * 0.8),
                ]
            };
            painter.add(Shape::convex_polygon(pts, theme.gutter_text, Stroke::NONE));
        }
    }
    if !options.line_numbers {
        return;
    }
    for l in lines {
        if st.layout.is_hidden(l) {
            continue;
        }
        let top = frame.origin.y + st.layout.line_top(l) + frame.text_dy;
        let color = if l == cursor_line {
            theme.gutter_current_text
        } else {
            theme.gutter_text
        };
        let galley = painter.layout_no_wrap((l + 1).to_string(), frame.font_id.clone(), color);
        let x = g.right() - 6.0 - fold_w - galley.size().x;
        painter.galley(pos2(x, top), galley, color);
    }
}

fn hover_tooltips(
    ui: &Ui,
    st: &mut EditorState,
    lang: &dyn EditorLanguage,
    frame: &Frame,
    response: &Response,
    gutter: &Response,
) {
    // Gutter markers.
    if gutter.hovered()
        && let Some(p) = gutter.hover_pos()
    {
        let line = st.layout.line_at_y(p.y - frame.origin.y);
        let msgs: Vec<(Severity, String)> = st
            .diagnostics
            .iter()
            .filter(|d| st.index.line_of_offset(d.range.start) == line)
            .map(|d| (d.severity, d.message.clone()))
            .collect();
        if !msgs.is_empty() {
            gutter
                .clone()
                .on_hover_ui_at_pointer(|ui| diag_list(ui, &msgs));
        }
        return;
    }
    if !response.hovered() || ui.input(|i| i.pointer.any_down()) {
        return;
    }
    let Some(p) = response.hover_pos() else {
        return;
    };
    if p.x < frame.text_left {
        return;
    }
    let doc = p - frame.origin;
    let line = st.layout.line_at_y(doc.y);
    let ctx = ui.ctx().clone();
    let g = st.layout.galley(&ctx, &st.text, &st.index, line);
    // Only when the pointer is over text, not past the line end.
    if doc.x > g.size().x + frame.char_w {
        return;
    }
    let (_, col, _) = st.layout.hit(&ctx, &st.text, &st.index, doc);
    let off = st.index.offset_of_char_col(&st.text, line, col);
    let msgs: Vec<(Severity, String)> = st
        .diagnostics
        .iter()
        .filter(|d| d.range.start <= off && off <= d.range.end.max(d.range.start + 1))
        .map(|d| (d.severity, d.message.clone()))
        .collect();
    let word = word_range_at(&st.text, off, |c| lang.is_word_char(c));
    let doc_text = match &word {
        Some(w) => {
            let version = st.version();
            let still = ui.input(|i| i.pointer.time_since_last_movement())
                >= ui.style().interaction.tooltip_delay;
            match &st.hover_cache {
                Some((v, r, s)) if *v == version && r == w => s.clone(),
                _ if still => {
                    let s = lang
                        .hover(&st.text, w.start)
                        .filter(|s| !s.trim().is_empty());
                    st.hover_cache = Some((version, w.clone(), s.clone()));
                    s
                }
                _ => None,
            }
        }
        None => None,
    };
    if msgs.is_empty() && doc_text.is_none() {
        return;
    }
    response.clone().on_hover_ui_at_pointer(|ui| {
        if !msgs.is_empty() {
            diag_list(ui, &msgs);
        }
        if let Some(t) = &doc_text {
            if !msgs.is_empty() {
                ui.separator();
            }
            ui.label(t);
        }
    });
}

fn diag_list(ui: &mut Ui, msgs: &[(Severity, String)]) {
    use egui_phosphor::regular as ph;
    let theme = EditorTheme::from_visuals(ui.visuals());
    for (sev, msg) in msgs {
        ui.horizontal_wrapped(|ui| {
            let (icon, color, label) = match sev {
                Severity::Error => (ph::X_CIRCLE, theme.error, tr("Error")),
                Severity::Warning => (ph::WARNING, theme.warning, tr("Warning")),
                Severity::Info => (ph::INFO, theme.info, tr("Info")),
            };
            ui.colored_label(color, icon).on_hover_text(label);
            ui.label(msg);
        });
    }
}

fn context_menu(
    st: &mut EditorState,
    ctx: &Context,
    lang: &dyn EditorLanguage,
    options: &EditorOptions,
    response: &Response,
    app_actions: &[ContextAction],
    events: &mut Vec<EditorEvent>,
) {
    let mut chosen: Option<ContextAction> = None;
    let has_sel = !st.sel.is_empty();
    let ro = options.read_only;
    let spell = st.spell_menu.clone();
    response.context_menu(|ui| {
        ui.set_min_width(200.0);
        let mut item = |ui: &mut Ui,
                        action: ContextAction,
                        enabled: bool,
                        shortcut: Option<&[egui::KeyboardShortcut]>| {
            let mut button = egui::Button::new(format!("{}  {}", action.icon(), action.label()));
            if let Some(sc) = shortcut.and_then(|s| s.first()) {
                button = button.shortcut_text(ctx.format_shortcut(sc));
            }
            if ui.add_enabled(enabled, button).clicked() {
                chosen = Some(action);
                ui.close();
            }
        };
        if let Some((range, word, suggestions)) = &spell {
            if suggestions.is_empty() {
                ui.add_enabled(false, egui::Label::new(tr("No suggestions")));
            }
            for s in suggestions.iter().take(8) {
                item(
                    ui,
                    ContextAction::ReplaceWord {
                        range: range.clone(),
                        with: s.clone(),
                    },
                    !ro,
                    None,
                );
            }
            item(ui, ContextAction::AddSpellingWord(word.clone()), true, None);
            ui.separator();
        }
        if !app_actions.is_empty() {
            for a in app_actions {
                item(ui, a.clone(), !(ro && a.is_mutating()), None);
            }
            ui.separator();
        }
        let keys = &options.keys;
        item(
            ui,
            ContextAction::Cut,
            !ro && (has_sel || options.copy_line_without_selection),
            None,
        );
        item(
            ui,
            ContextAction::Copy,
            has_sel || options.copy_line_without_selection,
            None,
        );
        item(ui, ContextAction::Paste, !ro, None);
        item(ui, ContextAction::SelectAll, true, Some(&keys.select_all));
        ui.separator();
        item(
            ui,
            ContextAction::ToggleComment,
            !ro,
            Some(&keys.toggle_comment),
        );
        item(ui, ContextAction::UpperCase, !ro, Some(&keys.upper_case));
        item(ui, ContextAction::LowerCase, !ro, Some(&keys.lower_case));
        ui.separator();
        item(ui, ContextAction::Find, true, Some(&keys.find));
        item(ui, ContextAction::GotoLine, true, Some(&keys.goto_line));
    });
    if let Some(action) = chosen {
        if action.is_editor_action() {
            input::apply_context_action(st, ctx, lang, options, &action);
        }
        st.pending_focus = true;
        events.push(EditorEvent::ContextMenu(action));
        ctx.request_repaint();
    }
}
