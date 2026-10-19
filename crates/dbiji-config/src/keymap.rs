//! Keyboard shortcuts: the [`Shortcut`] type, the DBiji and DBeaver keymap
//! presets, user overrides (`keymap.json`) and conflict detection.
//!
//! # Modifiers
//!
//! `command` is the platform's primary modifier: ⌘ on macOS, Ctrl elsewhere
//! (egui's `Modifiers::command`). `ctrl` is the physical Control key; it only
//! differs from `command` on macOS. Bindings are written once in portable
//! form (`Cmd+Shift+F`) and shown per platform (`⇧⌘F` / `Ctrl+Shift+F`).
//!
//! # Key names
//!
//! Keys use the names returned by egui's `Key::name()` (`Enter`, `F`,
//! `OpenBracket`, `Up`, digits as `"6"` ...), so the UI can convert them with
//! `egui::Key::from_name`. The parser also accepts symbols and aliases
//! (`↩`, `Return`, `[`, `Num6`, `ArrowUp`, `Esc` ...).

use crate::actions::{Action, ActionContext};
use crate::paths::{AppPaths, backup_corrupt_file, write_json};
use dbiji_core::DbResult;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::BTreeMap;
use std::fmt;

/// True when compiled for macOS (display and normalization default).
pub const IS_MAC: bool = cfg!(target_os = "macos");

/// A key combination.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Shortcut {
    /// egui key name (`Key::name()`), e.g. `"Enter"`, `"F"`, `"OpenBracket"`, `"6"`.
    pub key: String,
    /// ⌘ on macOS, Ctrl elsewhere.
    pub command: bool,
    /// Physical Control key (distinct from `command` on macOS only).
    pub ctrl: bool,
    /// Alt / ⌥ Option.
    pub alt: bool,
    /// Shift / ⇧.
    pub shift: bool,
}

/// (canonical egui name, mac symbol, other-platform label, extra aliases)
const KEYS: &[(&str, &str, &str, &[&str])] = &[
    (
        "Enter",
        "↩",
        "Enter",
        &["return", "↩", "⏎", "⌅", "numpadenter"],
    ),
    ("Escape", "⎋", "Esc", &["esc", "⎋"]),
    ("Tab", "⇥", "Tab", &["⇥"]),
    ("Backspace", "⌫", "Backspace", &["⌫"]),
    ("Delete", "⌦", "Delete", &["del", "⌦"]),
    ("Insert", "Ins", "Insert", &["ins", "help"]),
    ("Space", "Space", "Space", &["spacebar", " "]),
    ("Up", "↑", "Up", &["arrowup", "↑", "⏶"]),
    ("Down", "↓", "Down", &["arrowdown", "↓", "⏷"]),
    ("Left", "←", "Left", &["arrowleft", "←", "⏴"]),
    ("Right", "→", "Right", &["arrowright", "→", "⏵"]),
    ("Home", "↖", "Home", &["↖"]),
    ("End", "↘", "End", &["↘"]),
    ("PageUp", "⇞", "PageUp", &["pgup", "⇞"]),
    ("PageDown", "⇟", "PageDown", &["pgdn", "pagedn", "⇟"]),
    ("Copy", "Copy", "Copy", &[]),
    ("Cut", "Cut", "Cut", &[]),
    ("Paste", "Paste", "Paste", &[]),
    ("Colon", ":", ":", &[":"]),
    ("Comma", ",", ",", &[","]),
    ("Minus", "-", "-", &["-", "−", "numpadsubtract"]),
    ("Period", ".", ".", &[".", "dot"]),
    ("Plus", "+", "Plus", &["+", "numpadadd"]),
    ("Equals", "=", "=", &["=", "equal"]),
    ("Semicolon", ";", ";", &[";"]),
    ("Backslash", "\\", "\\", &["\\"]),
    ("Slash", "/", "/", &["/"]),
    ("Pipe", "|", "|", &["|"]),
    ("Questionmark", "?", "?", &["?"]),
    ("Exclamationmark", "!", "!", &["!"]),
    ("OpenBracket", "[", "[", &["[", "bracketleft"]),
    ("CloseBracket", "]", "]", &["]", "bracketright"]),
    ("OpenCurlyBracket", "{", "{", &["{"]),
    ("CloseCurlyBracket", "}", "}", &["}"]),
    ("Backtick", "`", "`", &["`", "backquote", "grave"]),
    ("Quote", "'", "'", &["'", "apostrophe"]),
];

/// Canonical egui key name for a user supplied key name, symbol or alias.
pub fn canonical_key(name: &str) -> Option<String> {
    let lower = name.to_lowercase();
    let trimmed = lower.trim();
    let token = if trimmed.is_empty() && !name.is_empty() {
        " "
    } else {
        trimmed
    };
    for (canon, _, _, aliases) in KEYS {
        if canon.eq_ignore_ascii_case(token) || aliases.contains(&token) {
            return Some((*canon).to_string());
        }
    }
    let mut chars = token.chars();
    if let (Some(c), None) = (chars.next(), chars.next()) {
        if c.is_ascii_alphabetic() {
            return Some(c.to_ascii_uppercase().to_string());
        }
        if c.is_ascii_digit() {
            return Some(c.to_string());
        }
    }
    for prefix in ["num", "digit", "numpad", "key"] {
        if let Some(rest) = token.strip_prefix(prefix) {
            let mut cs = rest.chars();
            if let (Some(c), None) = (cs.next(), cs.next()) {
                if c.is_ascii_digit() {
                    return Some(c.to_string());
                }
                if prefix == "key" && c.is_ascii_alphabetic() {
                    return Some(c.to_ascii_uppercase().to_string());
                }
            }
        }
    }
    if let Some(n) = token.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
        && (1..=35).contains(&n)
    {
        return Some(format!("F{n}"));
    }
    None
}

fn key_label(key: &str, mac: bool) -> String {
    for (canon, sym, other, _) in KEYS {
        if *canon == key {
            return if mac {
                (*sym).to_string()
            } else {
                (*other).to_string()
            };
        }
    }
    key.to_string()
}

impl Shortcut {
    /// Shortcut with the primary modifier (⌘ / Ctrl).
    pub fn cmd(key: &str) -> Shortcut {
        Shortcut {
            key: canonical_key(key).unwrap_or_else(|| key.to_string()),
            command: true,
            ..Default::default()
        }
    }

    /// Builds a shortcut from an input event. On non-mac platforms the
    /// physical Ctrl key is stored as `command` so bindings stay portable.
    pub fn from_event(
        key_name: &str,
        command: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
        mac: bool,
    ) -> Option<Shortcut> {
        let key = canonical_key(key_name)?;
        let s = Shortcut {
            key,
            command,
            ctrl,
            alt,
            shift,
        };
        Some(s.normalized(mac))
    }

    /// Parses `⇧⌘F`, `Ctrl+Shift+F`, `Cmd+Enter`, `Alt+X`, `⌃Space`, `Ctrl++` ...
    /// `Ctrl` means the physical Control key, `Cmd`/`Command`/`Meta`/`Super`
    /// the primary modifier.
    pub fn parse(text: &str) -> Option<Shortcut> {
        let text = text.trim();
        if text.is_empty() {
            return None;
        }
        let mut s = Shortcut::default();
        // Leading macOS modifier symbols.
        let mut rest = text;
        loop {
            let mut chars = rest.chars();
            let Some(c) = chars.next() else { break };
            let taken = match c {
                '⌘' => {
                    s.command = true;
                    true
                }
                '⌃' => {
                    s.ctrl = true;
                    true
                }
                '⌥' => {
                    s.alt = true;
                    true
                }
                '⇧' => {
                    s.shift = true;
                    true
                }
                _ => false,
            };
            if !taken {
                break;
            }
            rest = chars.as_str();
        }
        let rest = rest.trim();
        if rest.is_empty() {
            return None;
        }
        if rest == "+" || !rest.contains('+') {
            s.key = canonical_key(rest)?;
            return Some(s);
        }
        // Word syntax; a trailing "++" means the Plus key.
        let rest = rest.trim_start_matches('+');
        let (mods, key) = if let Some(m) = rest.strip_suffix("++") {
            (m, "+")
        } else {
            rest.rsplit_once('+').unwrap_or(("", rest))
        };
        for m in mods.split('+').map(str::trim).filter(|m| !m.is_empty()) {
            match m.to_lowercase().as_str() {
                "cmd" | "command" | "⌘" | "meta" | "super" | "win" | "primary" | "mod" => {
                    s.command = true
                }
                "ctrl" | "control" | "ctl" | "⌃" => s.ctrl = true,
                "alt" | "option" | "opt" | "⌥" => s.alt = true,
                "shift" | "⇧" => s.shift = true,
                _ => return None,
            }
        }
        s.key = canonical_key(key)?;
        Some(s)
    }

    /// Platform normalized form: on non-mac platforms Ctrl and the primary
    /// modifier are the same key, so `ctrl` is folded into `command`.
    pub fn normalized(&self, mac: bool) -> Shortcut {
        let mut s = self.clone();
        if !mac && s.ctrl {
            s.ctrl = false;
            s.command = true;
        }
        s
    }

    /// Display text: `⌃⌥⇧⌘F` order on macOS, `Ctrl+Alt+Shift+F` elsewhere.
    pub fn format(&self, mac: bool) -> String {
        if mac {
            let mut out = String::new();
            if self.ctrl {
                out.push('⌃');
            }
            if self.alt {
                out.push('⌥');
            }
            if self.shift {
                out.push('⇧');
            }
            if self.command {
                out.push('⌘');
            }
            out.push_str(&key_label(&self.key, true));
            out
        } else {
            let mut parts: Vec<String> = Vec::new();
            if self.command || self.ctrl {
                parts.push("Ctrl".into());
            }
            if self.alt {
                parts.push("Alt".into());
            }
            if self.shift {
                parts.push("Shift".into());
            }
            parts.push(key_label(&self.key, false));
            parts.join("+")
        }
    }

    /// Unambiguous, platform independent text used in `keymap.json`, e.g. `Cmd+Shift+F`.
    pub fn to_portable_string(&self) -> String {
        let mut parts: Vec<&str> = Vec::new();
        if self.command {
            parts.push("Cmd");
        }
        if self.ctrl {
            parts.push("Ctrl");
        }
        if self.alt {
            parts.push("Alt");
        }
        if self.shift {
            parts.push("Shift");
        }
        parts.push(&self.key);
        parts.join("+")
    }

    /// True when an input event (egui style modifiers) triggers this shortcut.
    pub fn matches(
        &self,
        key_name: &str,
        command: bool,
        ctrl: bool,
        alt: bool,
        shift: bool,
        mac: bool,
    ) -> bool {
        match Shortcut::from_event(key_name, command, ctrl, alt, shift, mac) {
            Some(ev) => ev == self.normalized(mac),
            None => false,
        }
    }
}

impl fmt::Display for Shortcut {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.format(IS_MAC))
    }
}

impl Serialize for Shortcut {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.to_portable_string())
    }
}

impl<'de> Deserialize<'de> for Shortcut {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Text(String),
            Full {
                key: String,
                #[serde(default)]
                command: bool,
                #[serde(default)]
                ctrl: bool,
                #[serde(default)]
                alt: bool,
                #[serde(default)]
                shift: bool,
            },
        }
        match Repr::deserialize(d)? {
            Repr::Text(t) => Shortcut::parse(&t)
                .ok_or_else(|| serde::de::Error::custom(format!("invalid shortcut '{t}'"))),
            Repr::Full {
                key,
                command,
                ctrl,
                alt,
                shift,
            } => {
                let key = canonical_key(&key)
                    .ok_or_else(|| serde::de::Error::custom(format!("invalid key '{key}'")))?;
                Ok(Shortcut {
                    key,
                    command,
                    ctrl,
                    alt,
                    shift,
                })
            }
        }
    }
}

/// Built-in keymap preset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeymapPreset {
    /// DBiji defaults (PRD §10).
    #[default]
    #[serde(alias = "DBiji")]
    Dbiji,
    /// DBeaver / Eclipse style defaults (Ctrl → ⌘ on macOS).
    #[serde(alias = "DBeaver")]
    Dbeaver,
}

impl KeymapPreset {
    /// Both presets.
    pub const ALL: [KeymapPreset; 2] = [KeymapPreset::Dbiji, KeymapPreset::Dbeaver];

    /// Display name.
    pub fn label(self) -> &'static str {
        match self {
            KeymapPreset::Dbiji => "DBiji",
            KeymapPreset::Dbeaver => "DBeaver",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Platform {
    All,
    Mac,
    Other,
}

use Platform::{All, Mac, Other};

const DBIJI_DEFAULTS: &[(Action, &str, Platform)] = &[
    (Action::OpenDatabaseObject, "Cmd+Shift+D", All),
    (Action::NextStatement, "Cmd+Alt+Down", All),
    (Action::PreviousStatement, "Cmd+Alt+Up", All),
    (Action::OpenDeclaration, "F3", Mac),
    (Action::ExecuteStatement, "Cmd+Enter", All),
    (Action::ExecuteScript, "Alt+X", All),
    (Action::ExecuteInNewTab, "Cmd+Shift+Enter", All),
    (Action::ExecuteInNewTab, "Cmd+Backslash", All),
    (Action::ExplainPlan, "Cmd+Shift+E", All),
    (Action::ExplainAnalyze, "Cmd+Alt+Shift+E", All),
    (Action::CancelQuery, "Cmd+Period", All),
    (Action::NewSqlEditor, "Cmd+CloseBracket", All),
    (Action::OpenSqlConsole, "Cmd+Shift+CloseBracket", All),
    (Action::SelectConnection, "Cmd+9", All),
    (Action::SelectSchema, "Cmd+0", All),
    (Action::ToggleResultsPanel, "Cmd+6", All),
    (Action::MaximizeResults, "Cmd+Shift+6", All),
    (Action::FormatSql, "Cmd+Shift+F", All),
    (Action::CommandPalette, "Cmd+Shift+P", All),
    (Action::Save, "Cmd+S", All),
    (Action::SaveData, "Cmd+S", All),
    (Action::Find, "Cmd+F", All),
    (Action::FindNext, "Cmd+G", Mac),
    (Action::FindNext, "F3", Other),
    (Action::FindPrevious, "Cmd+Shift+G", Mac),
    (Action::FindPrevious, "Shift+F3", Other),
    (Action::Replace, "Cmd+Alt+F", Mac),
    (Action::Replace, "Cmd+H", Other),
    (Action::GoToLine, "Cmd+L", All),
    (Action::ToggleComment, "Cmd+Slash", All),
    (Action::ToggleBlockComment, "Cmd+Shift+Slash", All),
    (Action::Autocomplete, "Ctrl+Space", All),
    (Action::UpperCase, "Cmd+Shift+X", All),
    (Action::LowerCase, "Cmd+Shift+Y", All),
    (Action::CloseTab, "Cmd+W", All),
    (Action::NextTab, "Ctrl+Tab", All),
    (Action::PrevTab, "Ctrl+Shift+Tab", All),
    (Action::Refresh, "F5", All),
    (Action::NewConnection, "Cmd+Shift+N", All),
    (Action::EditConnection, "F4", All),
    (Action::ToggleNavigator, "Cmd+B", All),
    (Action::OpenHistory, "Cmd+Shift+H", All),
    (Action::AiAssistant, "Cmd+I", All),
    (Action::OpenSettings, "Cmd+Comma", All),
    (Action::Quit, "Cmd+Q", All),
    (Action::ZoomIn, "Cmd+Equals", All),
    (Action::ZoomIn, "Cmd+Plus", All),
    (Action::ZoomOut, "Cmd+Minus", All),
    (Action::Help, "F1", All),
    (Action::ShowShortcuts, "Cmd+Shift+L", All),
];

/// DBeaver's documented defaults (Eclipse heritage); Ctrl maps to ⌘ on macOS
/// except for completion (Ctrl+Space).
const DBEAVER_DEFAULTS: &[(Action, &str, Platform)] = &[
    (Action::OpenDatabaseObject, "Cmd+Shift+D", All),
    (Action::NextStatement, "Cmd+Alt+Down", All),
    (Action::PreviousStatement, "Cmd+Alt+Up", All),
    (Action::OpenDeclaration, "F3", Mac),
    (Action::ExecuteStatement, "Cmd+Enter", All),
    (Action::ExecuteScript, "Alt+X", All),
    (Action::ExecuteInNewTab, "Cmd+Backslash", All),
    (Action::ExplainPlan, "Cmd+Shift+E", All),
    (Action::NewSqlEditor, "Cmd+CloseBracket", All),
    (Action::SelectConnection, "Cmd+9", All),
    (Action::SelectSchema, "Cmd+0", All),
    (Action::ToggleResultsPanel, "Cmd+6", All),
    (Action::FormatSql, "Cmd+Shift+F", All),
    (Action::CommandPalette, "Cmd+3", All),
    (Action::Save, "Cmd+S", All),
    (Action::SaveData, "Cmd+S", All),
    (Action::Find, "Cmd+F", All),
    (Action::FindNext, "Cmd+K", All),
    (Action::FindPrevious, "Cmd+Shift+K", All),
    (Action::GoToLine, "Cmd+L", All),
    (Action::ToggleComment, "Cmd+Slash", All),
    (Action::ToggleBlockComment, "Cmd+Shift+Slash", All),
    (Action::Autocomplete, "Ctrl+Space", All),
    (Action::UpperCase, "Cmd+Shift+X", All),
    (Action::LowerCase, "Cmd+Shift+Y", All),
    (Action::CloseTab, "Cmd+W", All),
    (Action::NextTab, "Cmd+PageDown", All),
    (Action::PrevTab, "Cmd+PageUp", All),
    (Action::Refresh, "F5", All),
    (Action::EditConnection, "F4", All),
    (Action::OpenSettings, "Cmd+Comma", Mac),
    (Action::Quit, "Cmd+Q", Mac),
    (Action::ZoomIn, "Cmd+Equals", All),
    (Action::ZoomIn, "Cmd+Plus", All),
    (Action::ZoomOut, "Cmd+Minus", All),
    (Action::Help, "F1", All),
    (Action::ShowShortcuts, "Cmd+Shift+L", All),
];

fn preset_defaults(preset: KeymapPreset, mac: bool) -> BTreeMap<Action, Vec<Shortcut>> {
    let table = match preset {
        KeymapPreset::Dbiji => DBIJI_DEFAULTS,
        KeymapPreset::Dbeaver => DBEAVER_DEFAULTS,
    };
    let mut map: BTreeMap<Action, Vec<Shortcut>> = BTreeMap::new();
    for (action, text, platform) in table {
        let applies = match platform {
            All => true,
            Mac => mac,
            Other => !mac,
        };
        if !applies {
            continue;
        }
        if let Some(s) = Shortcut::parse(text) {
            map.entry(*action).or_default().push(s);
        } else {
            tracing::error!("invalid built-in shortcut '{text}' for {}", action.id());
        }
    }
    map
}

/// Persisted keymap choice: preset plus per-action overrides (`keymap.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct KeymapConfig {
    /// Base preset.
    pub preset: KeymapPreset,
    /// Action id → shortcuts in portable syntax (`Cmd+Shift+F`). An empty
    /// list unbinds the action.
    pub overrides: BTreeMap<String, Vec<String>>,
}

/// Two or more actions sharing a shortcut in overlapping contexts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeymapConflict {
    /// The shared shortcut (platform normalized).
    pub shortcut: Shortcut,
    /// Actions bound to it.
    pub actions: Vec<Action>,
}

/// Effective keymap: preset defaults with overrides applied.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Keymap {
    preset: KeymapPreset,
    mac: bool,
    bindings: BTreeMap<Action, Vec<Shortcut>>,
}

impl Keymap {
    /// Defaults of `preset` for macOS (`mac = true`) or other platforms.
    pub fn new(preset: KeymapPreset, mac: bool) -> Keymap {
        Keymap {
            preset,
            mac,
            bindings: preset_defaults(preset, mac),
        }
    }

    /// Defaults of `preset` for the platform DBiji was compiled for.
    pub fn for_current_platform(preset: KeymapPreset) -> Keymap {
        Keymap::new(preset, IS_MAC)
    }

    /// Applies a persisted configuration. Unknown action ids and invalid
    /// shortcuts are skipped and reported in the returned warnings.
    pub fn from_config(cfg: &KeymapConfig, mac: bool) -> (Keymap, Vec<String>) {
        let mut km = Keymap::new(cfg.preset, mac);
        let mut warnings = Vec::new();
        for (id, texts) in &cfg.overrides {
            let Some(action) = Action::from_id(id) else {
                warnings.push(format!("keymap: unknown action '{id}' ignored"));
                continue;
            };
            let mut list = Vec::new();
            for t in texts {
                match Shortcut::parse(t) {
                    Some(s) => {
                        if !list.contains(&s) {
                            list.push(s);
                        }
                    }
                    None => {
                        warnings.push(format!("keymap: invalid shortcut '{t}' for '{id}' ignored"))
                    }
                }
            }
            km.bindings.insert(action, list);
        }
        km.bindings.retain(|_, v| !v.is_empty());
        (km, warnings)
    }

    /// The configuration that reproduces this keymap (only differences from the preset).
    pub fn to_config(&self) -> KeymapConfig {
        let defaults = preset_defaults(self.preset, self.mac);
        let mut overrides = BTreeMap::new();
        for action in Action::ALL {
            let cur = self.bindings.get(action).map(Vec::as_slice).unwrap_or(&[]);
            let def = defaults.get(action).map(Vec::as_slice).unwrap_or(&[]);
            if cur != def {
                overrides.insert(
                    action.id().to_string(),
                    cur.iter().map(Shortcut::to_portable_string).collect(),
                );
            }
        }
        KeymapConfig {
            preset: self.preset,
            overrides,
        }
    }

    /// The preset this keymap is based on.
    pub fn preset(&self) -> KeymapPreset {
        self.preset
    }

    /// Platform the keymap is resolved for.
    pub fn is_mac(&self) -> bool {
        self.mac
    }

    /// Shortcuts bound to `action` (possibly none).
    pub fn shortcuts(&self, action: Action) -> &[Shortcut] {
        self.bindings.get(&action).map(Vec::as_slice).unwrap_or(&[])
    }

    /// First shortcut of `action`, shown in menus.
    pub fn primary(&self, action: Action) -> Option<&Shortcut> {
        self.shortcuts(action).first()
    }

    /// Display text of the primary shortcut for this keymap's platform.
    pub fn label(&self, action: Action) -> Option<String> {
        self.primary(action).map(|s| s.format(self.mac))
    }

    /// Every bound action with its shortcuts.
    pub fn bindings(&self) -> impl Iterator<Item = (Action, &[Shortcut])> {
        self.bindings.iter().map(|(a, s)| (*a, s.as_slice()))
    }

    /// Replaces the shortcuts of `action` (empty = unbound).
    pub fn set(&mut self, action: Action, shortcuts: Vec<Shortcut>) {
        let mut dedup: Vec<Shortcut> = Vec::new();
        for s in shortcuts {
            if !dedup.contains(&s) {
                dedup.push(s);
            }
        }
        if dedup.is_empty() {
            self.bindings.remove(&action);
        } else {
            self.bindings.insert(action, dedup);
        }
    }

    /// Adds a shortcut to `action`.
    pub fn add(&mut self, action: Action, shortcut: Shortcut) {
        let list = self.bindings.entry(action).or_default();
        if !list.contains(&shortcut) {
            list.push(shortcut);
        }
    }

    /// Removes every shortcut of `action`.
    pub fn unbind(&mut self, action: Action) {
        self.bindings.remove(&action);
    }

    /// Restores the preset default of `action`.
    pub fn reset(&mut self, action: Action) {
        match preset_defaults(self.preset, self.mac).remove(&action) {
            Some(d) => {
                self.bindings.insert(action, d);
            }
            None => {
                self.bindings.remove(&action);
            }
        }
    }

    /// Restores every default of the preset.
    pub fn reset_all(&mut self) {
        self.bindings = preset_defaults(self.preset, self.mac);
    }

    /// Switches to another preset, discarding overrides.
    pub fn set_preset(&mut self, preset: KeymapPreset) {
        self.preset = preset;
        self.reset_all();
    }

    /// Action triggered by `shortcut` when focus is in `context`; an action
    /// of the focused context wins over a global one.
    pub fn action_for(&self, shortcut: &Shortcut, context: ActionContext) -> Option<Action> {
        let wanted = shortcut.normalized(self.mac);
        let mut global = None;
        for (action, list) in &self.bindings {
            if !list.iter().any(|s| s.normalized(self.mac) == wanted) {
                continue;
            }
            if action.context() == context {
                return Some(*action);
            }
            if action.context() == ActionContext::Global && global.is_none() {
                global = Some(*action);
            }
        }
        global
    }

    /// Actions that would conflict if `shortcut` were bound to `action`.
    pub fn conflicts_with(&self, action: Action, shortcut: &Shortcut) -> Vec<Action> {
        let wanted = shortcut.normalized(self.mac);
        self.bindings
            .iter()
            .filter(|(a, list)| {
                **a != action
                    && a.context().overlaps(action.context())
                    && list.iter().any(|s| s.normalized(self.mac) == wanted)
            })
            .map(|(a, _)| *a)
            .collect()
    }

    /// Every shortcut bound to several actions in overlapping contexts.
    pub fn conflicts(&self) -> Vec<KeymapConflict> {
        let mut by_shortcut: BTreeMap<Shortcut, Vec<Action>> = BTreeMap::new();
        for (action, list) in &self.bindings {
            for s in list {
                let entry = by_shortcut.entry(s.normalized(self.mac)).or_default();
                if !entry.contains(action) {
                    entry.push(*action);
                }
            }
        }
        let mut out = Vec::new();
        for (shortcut, actions) in by_shortcut {
            if actions.len() < 2 {
                continue;
            }
            let mut involved: Vec<Action> = Vec::new();
            for (i, a) in actions.iter().enumerate() {
                for b in &actions[i + 1..] {
                    if a.context().overlaps(b.context()) {
                        for x in [*a, *b] {
                            if !involved.contains(&x) {
                                involved.push(x);
                            }
                        }
                    }
                }
            }
            if !involved.is_empty() {
                out.push(KeymapConflict {
                    shortcut,
                    actions: involved,
                });
            }
        }
        out
    }
}

/// Reads `keymap.json`. Missing → defaults; corrupt → backed up, defaults,
/// with a warning.
pub fn load_config(paths: &AppPaths) -> (KeymapConfig, Vec<String>) {
    let path = paths.keymap_file();
    let bytes = match std::fs::read(&path) {
        Ok(b) => b,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return (KeymapConfig::default(), Vec::new());
        }
        Err(e) => {
            return (
                KeymapConfig::default(),
                vec![format!("Cannot read {}: {e}", path.display())],
            );
        }
    };
    match serde_json::from_slice::<KeymapConfig>(&bytes) {
        Ok(cfg) => (cfg, Vec::new()),
        Err(e) => {
            let mut w = format!(
                "{} is corrupt ({e}); using the default keymap",
                path.display()
            );
            match backup_corrupt_file(&path) {
                Ok(b) => w.push_str(&format!(" (backup: {})", b.display())),
                Err(be) => w.push_str(&format!(" (backup failed: {be})")),
            }
            (KeymapConfig::default(), vec![w])
        }
    }
}

/// Writes `keymap.json` atomically (mode 0600).
pub fn save_config(paths: &AppPaths, cfg: &KeymapConfig) -> DbResult<()> {
    write_json(&paths.keymap_file(), cfg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sc(t: &str) -> Shortcut {
        Shortcut::parse(t).unwrap_or_else(|| panic!("parse {t}"))
    }

    #[test]
    fn parse_word_and_symbol_syntax() {
        let a = sc("Ctrl+Shift+F");
        assert_eq!(
            a,
            Shortcut {
                key: "F".into(),
                ctrl: true,
                shift: true,
                ..Default::default()
            }
        );
        let b = sc("⇧⌘F");
        assert_eq!(
            b,
            Shortcut {
                key: "F".into(),
                command: true,
                shift: true,
                ..Default::default()
            }
        );
        assert_eq!(
            sc("⌘↩"),
            Shortcut {
                key: "Enter".into(),
                command: true,
                ..Default::default()
            }
        );
        assert_eq!(
            sc("⌥X"),
            Shortcut {
                key: "X".into(),
                alt: true,
                ..Default::default()
            }
        );
        assert_eq!(sc("⌘]").key, "CloseBracket");
        assert_eq!(sc("cmd+num6").key, "6");
        assert_eq!(sc("Alt+Return").key, "Enter");
        assert_eq!(sc("Ctrl++").key, "Plus");
        assert_eq!(sc("⌘+").key, "Plus");
        assert_eq!(sc("Cmd+OpenBracket").key, "OpenBracket");
        assert_eq!(
            sc("Option+ArrowUp"),
            Shortcut {
                key: "Up".into(),
                alt: true,
                ..Default::default()
            }
        );
        assert_eq!(sc("F12").key, "F12");
        assert_eq!(
            sc("⌃Space"),
            Shortcut {
                key: "Space".into(),
                ctrl: true,
                ..Default::default()
            }
        );
        assert!(Shortcut::parse("").is_none());
        assert!(Shortcut::parse("Hyper+F").is_none());
        assert!(Shortcut::parse("Ctrl+Foo").is_none());
        assert!(Shortcut::parse("⌘").is_none());
    }

    #[test]
    fn format_per_platform() {
        let s = sc("Cmd+Shift+F");
        assert_eq!(s.format(true), "⇧⌘F");
        assert_eq!(s.format(false), "Ctrl+Shift+F");
        assert_eq!(sc("Cmd+Enter").format(true), "⌘↩");
        assert_eq!(sc("Cmd+Enter").format(false), "Ctrl+Enter");
        assert_eq!(sc("Alt+X").format(true), "⌥X");
        assert_eq!(sc("Alt+X").format(false), "Alt+X");
        assert_eq!(sc("Cmd+CloseBracket").format(true), "⌘]");
        assert_eq!(sc("Ctrl+Alt+Shift+Cmd+K").format(true), "⌃⌥⇧⌘K");
        assert_eq!(sc("Ctrl+Space").format(false), "Ctrl+Space");
        assert_eq!(sc("Cmd+Plus").format(false), "Ctrl+Plus");
        // Display text parses back to the same shortcut (mac) / normalized (other).
        for t in [
            "Cmd+Shift+F",
            "Alt+X",
            "Cmd+9",
            "Ctrl+Space",
            "Cmd+Alt+Shift+E",
            "Cmd+Backslash",
            "Shift+F3",
        ] {
            let s = sc(t);
            assert_eq!(sc(&s.format(true)), s, "{t}");
            assert_eq!(
                sc(&s.format(false)).normalized(false),
                s.normalized(false),
                "{t}"
            );
            assert_eq!(sc(&s.to_portable_string()), s, "{t}");
        }
    }

    #[test]
    fn serde_forms() {
        let s = sc("Cmd+Shift+P");
        assert_eq!(serde_json::to_string(&s).unwrap(), "\"Cmd+Shift+P\"");
        let back: Shortcut = serde_json::from_str("\"⇧⌘P\"").unwrap();
        assert_eq!(back, s);
        let obj: Shortcut =
            serde_json::from_str(r#"{"key":"p","command":true,"shift":true}"#).unwrap();
        assert_eq!(obj, s);
        assert!(serde_json::from_str::<Shortcut>("\"Nope+Q\"").is_err());
    }

    #[test]
    fn event_matching_and_normalization() {
        let s = sc("Cmd+S");
        // egui on Windows/Linux reports ctrl = command = true for Ctrl+S.
        assert!(s.matches("S", true, true, false, false, false));
        // macOS ⌘S.
        assert!(s.matches("S", true, false, false, false, true));
        // macOS ⌃S is a different key.
        assert!(!s.matches("S", false, true, false, false, true));
        let ctrl_space = sc("Ctrl+Space");
        assert!(ctrl_space.matches("Space", false, true, false, false, true));
        assert!(ctrl_space.matches("Space", true, true, false, false, false));
        assert_eq!(
            Shortcut::from_event("s", true, true, false, false, false).unwrap(),
            sc("Cmd+S")
        );
    }

    #[test]
    fn dbiji_preset_matches_prd() {
        let km = Keymap::new(KeymapPreset::Dbiji, true);
        let label = |a| km.label(a).unwrap();
        assert_eq!(label(Action::ExecuteStatement), "⌘↩");
        assert_eq!(label(Action::ExecuteScript), "⌥X");
        assert_eq!(label(Action::NewSqlEditor), "⌘]");
        assert_eq!(label(Action::SelectConnection), "⌘9");
        assert_eq!(label(Action::SelectSchema), "⌘0");
        assert_eq!(label(Action::ToggleResultsPanel), "⌘6");
        assert_eq!(label(Action::FormatSql), "⇧⌘F");
        assert_eq!(label(Action::CommandPalette), "⇧⌘P");
        assert_eq!(label(Action::SaveData), "⌘S");
        let win = Keymap::new(KeymapPreset::Dbiji, false);
        assert_eq!(win.label(Action::FormatSql).unwrap(), "Ctrl+Shift+F");
        assert_eq!(win.label(Action::FindNext).unwrap(), "F3");
        assert_eq!(km.label(Action::FindNext).unwrap(), "⌘G");
    }

    #[test]
    fn presets_have_no_conflicts() {
        for preset in KeymapPreset::ALL {
            for mac in [true, false] {
                let km = Keymap::new(preset, mac);
                assert!(
                    km.conflicts().is_empty(),
                    "{preset:?} mac={mac}: {:?}",
                    km.conflicts()
                );
                // Every built-in entry parsed.
                let table = match preset {
                    KeymapPreset::Dbiji => DBIJI_DEFAULTS,
                    KeymapPreset::Dbeaver => DBEAVER_DEFAULTS,
                };
                for (_, t, _) in table {
                    assert!(Shortcut::parse(t).is_some(), "{t}");
                }
            }
        }
        let dbeaver = Keymap::new(KeymapPreset::Dbeaver, false);
        assert_eq!(
            dbeaver.label(Action::ExecuteStatement).unwrap(),
            "Ctrl+Enter"
        );
        assert_eq!(dbeaver.label(Action::Autocomplete).unwrap(), "Ctrl+Space");
        assert_eq!(dbeaver.label(Action::FindNext).unwrap(), "Ctrl+K");
        let dbeaver_mac = Keymap::new(KeymapPreset::Dbeaver, true);
        assert_eq!(dbeaver_mac.label(Action::ExecuteInNewTab).unwrap(), "⌘\\");
        assert_eq!(dbeaver_mac.label(Action::Autocomplete).unwrap(), "⌃Space");
    }

    #[test]
    fn context_aware_lookup() {
        let km = Keymap::new(KeymapPreset::Dbiji, true);
        let save = sc("Cmd+S");
        assert_eq!(
            km.action_for(&save, ActionContext::SqlEditor),
            Some(Action::Save)
        );
        assert_eq!(
            km.action_for(&save, ActionContext::ResultsGrid),
            Some(Action::SaveData)
        );
        assert_eq!(km.action_for(&save, ActionContext::Navigator), None);
        assert_eq!(
            km.action_for(&sc("Cmd+Shift+P"), ActionContext::ResultsGrid),
            Some(Action::CommandPalette)
        );
    }

    #[test]
    fn overrides_conflicts_and_roundtrip() {
        let mut km = Keymap::new(KeymapPreset::Dbiji, true);
        // Binding ⌘W (CloseTab, global) to FormatSql conflicts.
        assert_eq!(
            km.conflicts_with(Action::FormatSql, &sc("Cmd+W")),
            vec![Action::CloseTab]
        );
        km.set(Action::FormatSql, vec![sc("Cmd+W")]);
        let conflicts = km.conflicts();
        assert_eq!(conflicts.len(), 1);
        assert_eq!(conflicts[0].actions.len(), 2);
        km.unbind(Action::CloseTab);
        assert!(km.conflicts().is_empty());

        let cfg = km.to_config();
        assert_eq!(
            cfg.overrides.get("editor.format_sql").unwrap(),
            &vec!["Cmd+W".to_string()]
        );
        assert_eq!(
            cfg.overrides.get("tab.close").unwrap(),
            &Vec::<String>::new()
        );
        assert_eq!(cfg.overrides.len(), 2);
        let (back, warnings) = Keymap::from_config(&cfg, true);
        assert!(warnings.is_empty());
        assert_eq!(back, km);

        km.reset(Action::FormatSql);
        km.reset(Action::CloseTab);
        assert_eq!(km, Keymap::new(KeymapPreset::Dbiji, true));
        assert!(km.to_config().overrides.is_empty());

        let mut bad = KeymapConfig::default();
        bad.overrides.insert("nope".into(), vec!["Cmd+S".into()]);
        bad.overrides
            .insert("editor.find".into(), vec!["Hyper+Q".into(), "Cmd+F".into()]);
        let (km2, warnings) = Keymap::from_config(&bad, true);
        assert_eq!(warnings.len(), 2);
        assert_eq!(km2.shortcuts(Action::Find), &[sc("Cmd+F")]);
    }

    #[test]
    fn non_mac_ctrl_and_cmd_conflict() {
        let mut km = Keymap::new(KeymapPreset::Dbiji, false);
        km.set(Action::Help, vec![sc("Ctrl+W")]);
        // On Windows Ctrl+W == Cmd+W (CloseTab).
        assert_eq!(km.conflicts().len(), 1);
        let mut mac = Keymap::new(KeymapPreset::Dbiji, true);
        mac.set(Action::Help, vec![sc("Ctrl+W")]);
        assert!(mac.conflicts().is_empty());
    }

    #[test]
    fn load_and_save_keymap_file() {
        let dir = tempfile::tempdir().unwrap();
        let paths = AppPaths::with_root(dir.path());
        let (cfg, w) = load_config(&paths);
        assert_eq!(cfg, KeymapConfig::default());
        assert!(w.is_empty());
        let mut cfg = KeymapConfig {
            preset: KeymapPreset::Dbeaver,
            ..Default::default()
        };
        cfg.overrides
            .insert("sql.execute_statement".into(), vec!["Cmd+E".into()]);
        save_config(&paths, &cfg).unwrap();
        let (back, w) = load_config(&paths);
        assert!(w.is_empty());
        assert_eq!(back, cfg);
        std::fs::write(paths.keymap_file(), "{broken").unwrap();
        let (fallback, w) = load_config(&paths);
        assert_eq!(fallback, KeymapConfig::default());
        assert_eq!(w.len(), 1);
        assert!(!paths.keymap_file().exists());
        // Old capitalised preset names still load.
        std::fs::write(paths.keymap_file(), r#"{"preset":"DBeaver"}"#).unwrap();
        assert_eq!(load_config(&paths).0.preset, KeymapPreset::Dbeaver);
    }
}
