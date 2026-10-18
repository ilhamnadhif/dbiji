//! Application preferences (`settings.json`).
//!
//! Every struct uses `#[serde(default)]` and unknown fields are ignored, so
//! files written by older or newer versions keep loading. Enum values that a
//! version does not know fall back to their default instead of failing, and a
//! section with invalid content is reset on its own (with a warning) rather
//! than discarding the whole file. A file that is not valid JSON at all is
//! moved aside (`settings.json.corrupt-<timestamp>`) and defaults are used.
//!
//! Keyboard shortcuts are part of [`AppSettings`] but persisted separately in
//! `keymap.json` (see [`crate::keymap`]).

use crate::keymap::{self, KeymapConfig};
use crate::paths::{AppPaths, backup_corrupt_file, write_json};
use dbiji_core::DbResult;
use dbiji_core::config::NavigatorView;
use dbiji_core::i18n::Locale;
use dbiji_core::value::FormatOptions;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Deserializer, Serialize};

/// Current schema version written to `settings.json`.
pub const SETTINGS_VERSION: u32 = 1;

/// Deserializes a value, falling back to its default when it is invalid
/// (e.g. an enum variant written by a newer version).
fn lenient<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: Deserializer<'de>,
    T: DeserializeOwned + Default,
{
    let v = serde_json::Value::deserialize(d)?;
    Ok(serde_json::from_value(v).unwrap_or_default())
}

/// All preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct AppSettings {
    /// Schema version of the file.
    pub version: u32,
    /// Language, theme, startup behaviour.
    pub general: GeneralSettings,
    /// SQL editor.
    pub editor: EditorSettings,
    /// Result sets and data editor.
    pub results: ResultsSettings,
    /// Statement execution.
    pub execution: ExecutionSettings,
    /// Query manager (history).
    pub history: HistorySettings,
    /// Database navigator.
    pub navigator: NavigatorSettings,
    /// Keymap preset and overrides; stored in `keymap.json`.
    #[serde(skip)]
    pub keymap: KeymapConfig,
    /// Main toolbar.
    pub toolbar: ToolbarSettings,
    /// AI assistant.
    pub ai: AiSettings,
    /// Secret storage.
    pub security: SecuritySettings,
    /// Task scheduler.
    pub tasks: TaskSettings,
    /// Accessibility options.
    pub accessibility: AccessibilitySettings,
    /// Update checks and release notes.
    pub updates: UpdateSettings,
}

impl Default for AppSettings {
    fn default() -> Self {
        AppSettings {
            version: SETTINGS_VERSION,
            general: GeneralSettings::default(),
            editor: EditorSettings::default(),
            results: ResultsSettings::default(),
            execution: ExecutionSettings::default(),
            history: HistorySettings::default(),
            navigator: NavigatorSettings::default(),
            keymap: KeymapConfig::default(),
            toolbar: ToolbarSettings::default(),
            ai: AiSettings::default(),
            security: SecuritySettings::default(),
            tasks: TaskSettings::default(),
            accessibility: AccessibilitySettings::default(),
            updates: UpdateSettings::default(),
        }
    }
}

/// Colour theme.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum Theme {
    /// Follow the operating system (light / dark).
    #[default]
    System,
    /// Light theme.
    Light,
    /// Dark theme.
    Dark,
}

/// General preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct GeneralSettings {
    /// UI language; `None` follows the operating system.
    #[serde(deserialize_with = "lenient")]
    pub locale: Option<Locale>,
    /// Colour theme.
    #[serde(deserialize_with = "lenient")]
    pub theme: Theme,
    /// UI font size in points.
    pub ui_font_size: f32,
    /// Additional UI zoom factor (1.0 = 100 %).
    pub ui_scale: f32,
    /// Reopen the editors of the previous session on start.
    pub restore_session: bool,
    /// Ask before quitting with unsaved scripts or pending data changes.
    pub confirm_exit_with_unsaved: bool,
}

impl Default for GeneralSettings {
    fn default() -> Self {
        GeneralSettings {
            locale: None,
            theme: Theme::System,
            ui_font_size: 13.0,
            ui_scale: 1.0,
            restore_session: true,
            confirm_exit_with_unsaved: true,
        }
    }
}

impl GeneralSettings {
    /// The configured locale or, when unset, the operating system locale.
    pub fn effective_locale(&self) -> Locale {
        self.locale.unwrap_or_else(dbiji_core::i18n::system_locale)
    }
}

/// Case of SQL keywords (formatter and completion).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum KeywordCase {
    /// UPPER CASE.
    #[default]
    Upper,
    /// lower case.
    Lower,
    /// Keep what was typed.
    Preserve,
}

/// Whether a blank line ends a statement when executing "statement under cursor".
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum BlankLineDelimiter {
    /// Only the statement delimiter ends a statement.
    Never,
    /// A blank line ends a statement unless it is inside a block.
    #[default]
    Smart,
    /// Every blank line ends a statement.
    Always,
}

/// Completion popup.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AutocompleteSettings {
    /// Completion enabled.
    pub enabled: bool,
    /// Delay before the popup opens automatically.
    pub delay_ms: u32,
    /// Open the popup automatically while typing letters (not only after `.`).
    pub auto_activate_on_letters: bool,
    /// Characters typed before automatic activation.
    pub min_chars: u32,
    /// Insert the proposal directly when it is the only one.
    pub insert_single_proposal: bool,
    /// Case of completed keywords.
    #[serde(deserialize_with = "lenient")]
    pub keyword_case: KeywordCase,
    /// Maximum number of proposals shown.
    pub max_items: u32,
}

impl Default for AutocompleteSettings {
    fn default() -> Self {
        AutocompleteSettings {
            enabled: true,
            delay_ms: 100,
            auto_activate_on_letters: true,
            min_chars: 1,
            insert_single_proposal: true,
            keyword_case: KeywordCase::Upper,
            max_items: 200,
        }
    }
}

/// SQL formatter options.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SqlFormatSettings {
    /// Case of keywords produced by the formatter.
    #[serde(deserialize_with = "lenient")]
    pub keyword_case: KeywordCase,
    /// Spaces per indentation level.
    pub indent_width: u32,
    /// Indent with tabs instead of spaces.
    pub use_tabs: bool,
    /// Put commas at the start of lines in select lists.
    pub comma_first: bool,
    /// Break lines before `AND` / `OR`.
    pub newline_before_and_or: bool,
    /// Blank lines between formatted statements.
    pub lines_between_statements: u32,
    /// Preferred maximum line length.
    pub max_line_length: u32,
}

impl Default for SqlFormatSettings {
    fn default() -> Self {
        SqlFormatSettings {
            keyword_case: KeywordCase::Upper,
            indent_width: 4,
            use_tabs: false,
            comma_first: false,
            newline_before_and_or: true,
            lines_between_statements: 1,
            max_line_length: 120,
        }
    }
}

/// Query parameter / variable recognition.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ParameterSettings {
    /// Master switch.
    pub enabled: bool,
    /// `:name` parameters.
    pub named_colon: bool,
    /// `${var}` variables.
    pub dollar_brace: bool,
    /// Anonymous `?` parameters.
    pub question_mark: bool,
    /// Ask for values before executing.
    pub prompt_for_values: bool,
}

impl Default for ParameterSettings {
    fn default() -> Self {
        ParameterSettings {
            enabled: true,
            named_colon: true,
            dollar_brace: true,
            question_mark: false,
            prompt_for_values: true,
        }
    }
}

/// SQL editor preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct EditorSettings {
    /// Monospace font family; `None` = built-in font.
    pub font_family: Option<String>,
    /// Font size in points.
    pub font_size: f32,
    /// Width of a tab in spaces.
    pub tab_size: u32,
    /// Insert spaces when Tab is pressed.
    pub insert_spaces: bool,
    /// Completion.
    pub autocomplete: AutocompleteSettings,
    /// Close quotes automatically.
    pub auto_close_quotes: bool,
    /// Close brackets automatically.
    pub auto_close_brackets: bool,
    /// Highlight the line with the caret.
    pub highlight_current_line: bool,
    /// Show line numbers.
    pub line_numbers: bool,
    /// Wrap long lines.
    pub word_wrap: bool,
    /// Blank line handling for "execute statement".
    #[serde(deserialize_with = "lenient")]
    pub blank_line_delimiter: BlankLineDelimiter,
    /// Statement delimiter (`;` or e.g. `GO`, `$$`).
    pub statement_delimiter: String,
    /// Formatter options.
    pub format: SqlFormatSettings,
    /// Spell checking of comments and string literals.
    pub spell_check: bool,
    /// Automatic save of scripts; 0 disables.
    pub auto_save_interval_secs: u32,
    /// Parameter / variable recognition.
    pub parameters: ParameterSettings,
}

impl Default for EditorSettings {
    fn default() -> Self {
        EditorSettings {
            font_family: None,
            font_size: 13.0,
            tab_size: 4,
            insert_spaces: true,
            autocomplete: AutocompleteSettings::default(),
            auto_close_quotes: true,
            auto_close_brackets: true,
            highlight_current_line: true,
            line_numbers: true,
            word_wrap: false,
            blank_line_delimiter: BlankLineDelimiter::Smart,
            statement_delimiter: ";".into(),
            format: SqlFormatSettings::default(),
            spell_check: false,
            auto_save_interval_secs: 30,
            parameters: ParameterSettings::default(),
        }
    }
}

/// Position of the results panel relative to the SQL text.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ResultsLayout {
    /// Results below the SQL text.
    #[default]
    Bottom,
    /// Results to the right of the SQL text.
    Right,
}

/// Where grid sorting / filtering runs by default.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SortFilterMode {
    /// Re-query with ORDER BY / WHERE.
    #[default]
    Server,
    /// Sort / filter the fetched rows locally.
    Client,
}

/// Result set preferences.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct ResultsSettings {
    /// Rows fetched per page.
    pub fetch_size: u32,
    /// Text values longer than this (bytes) are truncated when fetched.
    pub max_text_length: usize,
    /// Data formatting.
    pub format: FormatOptions,
    /// Position of the results panel.
    #[serde(deserialize_with = "lenient")]
    pub layout: ResultsLayout,
    /// Show every executed query in a new results tab.
    pub open_each_query_in_new_tab: bool,
    /// Maximum number of results tabs per editor.
    pub max_result_tabs: u32,
    /// Show row numbers in grids.
    pub show_row_numbers: bool,
    /// Default place where sorting / filtering runs.
    #[serde(deserialize_with = "lenient")]
    pub sort_filter_mode: SortFilterMode,
    /// Default auto-refresh interval of result sets; 0 disables.
    pub auto_refresh_secs: u32,
    /// Memory budget of one result set before rows spill to disk.
    pub store_memory_limit_mb: u32,
    /// Memory limit of all result sets together.
    pub global_memory_limit_mb: u32,
    /// Ask before "fetch all rows" when more than this many rows are expected.
    pub confirm_fetch_all_above: u64,
}

impl Default for ResultsSettings {
    fn default() -> Self {
        ResultsSettings {
            fetch_size: 200,
            max_text_length: 1024 * 1024,
            format: FormatOptions::default(),
            layout: ResultsLayout::Bottom,
            open_each_query_in_new_tab: false,
            max_result_tabs: 10,
            show_row_numbers: true,
            sort_filter_mode: SortFilterMode::Server,
            auto_refresh_secs: 0,
            store_memory_limit_mb: 48,
            global_memory_limit_mb: 192,
            confirm_fetch_all_above: 100_000,
        }
    }
}

impl ResultsSettings {
    /// Per result set budget in bytes (at least 1 MiB).
    pub fn store_budget_bytes(&self) -> usize {
        (self.store_memory_limit_mb.max(1) as usize) * 1024 * 1024
    }

    /// Global row store limit in bytes (at least 16 MiB).
    pub fn global_limit_bytes(&self) -> usize {
        (self.global_memory_limit_mb.max(16) as usize) * 1024 * 1024
    }

    /// Applies the global memory limit to `dbiji_core::rowstore`.
    pub fn apply_memory_limits(&self) {
        dbiji_core::rowstore::set_global_limit(self.global_limit_bytes());
    }

    /// Fetch size, at least 1.
    pub fn effective_fetch_size(&self) -> u32 {
        self.fetch_size.max(1)
    }
}

/// Statement execution preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ExecutionSettings {
    /// Stop a script at the first failing statement.
    pub stop_on_error: bool,
    /// Confirm DELETE/UPDATE without WHERE, DROP, TRUNCATE (on top of the connection type).
    pub confirm_dangerous_statements: bool,
    /// Switch to manual commit automatically when a modifying statement runs.
    pub smart_commit: bool,
    /// Default statement timeout; 0 disables.
    pub query_timeout_secs: u32,
}

impl Default for ExecutionSettings {
    fn default() -> Self {
        ExecutionSettings {
            stop_on_error: true,
            confirm_dangerous_statements: true,
            smart_commit: false,
            query_timeout_secs: 0,
        }
    }
}

/// Query manager retention.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct HistorySettings {
    /// Record executed statements.
    pub enabled: bool,
    /// Keep at most this many entries; 0 = unlimited.
    pub max_entries: u64,
    /// Delete entries older than this; 0 = keep forever.
    pub max_age_days: u32,
    /// Mask passwords in statements before storing them (`ALTER USER ... PASSWORD '***'`).
    pub redact_secrets: bool,
}

impl Default for HistorySettings {
    fn default() -> Self {
        HistorySettings {
            enabled: true,
            max_entries: 50_000,
            max_age_days: 90,
            redact_secrets: true,
        }
    }
}

impl HistorySettings {
    /// Retention policy for [`crate::history::HistoryStore`].
    pub fn retention(&self) -> crate::history::RetentionPolicy {
        crate::history::RetentionPolicy {
            max_entries: (self.max_entries > 0).then_some(self.max_entries),
            max_age_days: (self.max_age_days > 0).then_some(self.max_age_days),
        }
    }
}

/// Navigator ordering.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum NavigatorSort {
    /// Case-insensitive alphabetical order.
    #[default]
    Alphabetical,
    /// Order reported by the database.
    Natural,
}

/// Navigator preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NavigatorSettings {
    /// Show system schemas and objects.
    pub show_system_objects: bool,
    /// Order of navigator children.
    #[serde(deserialize_with = "lenient")]
    pub sort: NavigatorSort,
    /// Show `user@host` next to connection names.
    pub show_connection_details: bool,
    /// View mode for new connections.
    #[serde(deserialize_with = "lenient")]
    pub default_view: NavigatorView,
    /// Expand a connection's node after connecting.
    pub expand_on_connect: bool,
}

impl Default for NavigatorSettings {
    fn default() -> Self {
        NavigatorSettings {
            show_system_objects: false,
            sort: NavigatorSort::Alphabetical,
            show_connection_details: true,
            default_view: NavigatorView::Advanced,
            expand_on_connect: true,
        }
    }
}

/// Main toolbar configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct ToolbarSettings {
    /// Ordered action ids ([`crate::actions::Action::id`]); `"|"` is a separator.
    pub items: Vec<String>,
    /// Action ids hidden from the toolbar.
    pub hidden: Vec<String>,
}

impl Default for ToolbarSettings {
    fn default() -> Self {
        ToolbarSettings {
            items: crate::actions::Action::default_toolbar()
                .iter()
                .map(|a| a.id().to_string())
                .collect(),
            hidden: Vec::new(),
        }
    }
}

impl ToolbarSettings {
    /// Visible actions in order (unknown ids and separators skipped).
    pub fn visible_actions(&self) -> Vec<crate::actions::Action> {
        self.items
            .iter()
            .filter(|id| !self.hidden.contains(id))
            .filter_map(|id| crate::actions::Action::from_id(id))
            .collect()
    }
}

/// AI provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum AiProvider {
    /// Anthropic Claude API.
    #[default]
    Anthropic,
    /// Any server implementing the OpenAI-compatible chat API (`base_url` required).
    OpenAiCompatible,
}

impl AiProvider {
    /// Identifier used in secret keys (`ai:<provider>:api-key`).
    pub fn id(self) -> &'static str {
        match self {
            AiProvider::Anthropic => "anthropic",
            AiProvider::OpenAiCompatible => "openai-compatible",
        }
    }
}

/// Reasoning effort requested from the model.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum AiEffort {
    /// Fast, minimal reasoning.
    Low,
    /// Balanced.
    Medium,
    /// Thorough (default).
    #[default]
    High,
    /// More thorough than high.
    Xhigh,
    /// Maximum effort.
    Max,
}

impl AiEffort {
    /// Value of the API parameter (`low` ... `max`).
    pub fn as_str(self) -> &'static str {
        match self {
            AiEffort::Low => "low",
            AiEffort::Medium => "medium",
            AiEffort::High => "high",
            AiEffort::Xhigh => "xhigh",
            AiEffort::Max => "max",
        }
    }
}

/// AI assistant. Disabled by default: no data leaves the machine unless enabled.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AiSettings {
    /// The assistant is enabled.
    pub enabled: bool,
    /// Model provider.
    #[serde(deserialize_with = "lenient")]
    pub provider: AiProvider,
    /// Model id.
    pub model: String,
    /// API endpoint; `None` = the provider's default endpoint.
    pub base_url: Option<String>,
    /// Maximum tokens per response.
    pub max_tokens: u32,
    /// Reasoning effort.
    #[serde(deserialize_with = "lenient")]
    pub effort: AiEffort,
    /// Send table / column names of the active schema as context.
    pub include_schema_context: bool,
    /// Send sample rows (off by default for privacy).
    pub send_sample_data: bool,
}

impl Default for AiSettings {
    fn default() -> Self {
        AiSettings {
            enabled: false,
            provider: AiProvider::Anthropic,
            model: "claude-opus-5-5".into(),
            base_url: None,
            max_tokens: 16_000,
            effort: AiEffort::High,
            include_schema_context: true,
            send_sample_data: false,
        }
    }
}

/// Where passwords and API keys are stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum SecretBackend {
    /// macOS Keychain / Windows Credential Manager / Secret Service.
    #[default]
    Keychain,
    /// `vault.bin` encrypted with a master password.
    EncryptedFile,
}

/// Secret storage preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SecuritySettings {
    /// Where secrets are stored.
    #[serde(deserialize_with = "lenient")]
    pub secret_backend: SecretBackend,
    /// Lock the encrypted vault after this many idle minutes; 0 = never.
    pub auto_lock_minutes: u32,
}

impl Default for SecuritySettings {
    fn default() -> Self {
        SecuritySettings {
            secret_backend: SecretBackend::Keychain,
            auto_lock_minutes: 30,
        }
    }
}

/// Task preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TaskSettings {
    /// Run scheduled tasks while DBiji is open.
    pub scheduler_enabled: bool,
}

impl Default for TaskSettings {
    fn default() -> Self {
        TaskSettings {
            scheduler_enabled: true,
        }
    }
}

/// Update checks and the "What's new" notes.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateSettings {
    /// Look for a newer release once a day; `None` until the user answered.
    pub check_automatically: Option<bool>,
    /// Release the user chose to skip.
    pub skipped_version: Option<String>,
    /// Last automatic check (Unix seconds).
    pub last_check: Option<i64>,
    /// Version whose release notes were last shown.
    pub last_seen_version: Option<String>,
}

/// Accessibility preferences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct AccessibilitySettings {
    /// High contrast colours.
    pub high_contrast: bool,
    /// Disable animations.
    pub reduce_motion: bool,
    /// Always draw a visible keyboard focus indicator.
    pub focus_indicator: bool,
}

impl Default for AccessibilitySettings {
    fn default() -> Self {
        AccessibilitySettings {
            high_contrast: false,
            reduce_motion: false,
            focus_indicator: true,
        }
    }
}

/// Loads the settings (and `keymap.json`). Never fails: problems are logged
/// and the affected parts fall back to defaults.
pub fn load(paths: &AppPaths) -> AppSettings {
    load_with_warnings(paths).0
}

/// Like [`load`], also returning the warnings for display in the UI.
pub fn load_with_warnings(paths: &AppPaths) -> (AppSettings, Vec<String>) {
    let path = paths.settings_file();
    let mut warnings = Vec::new();
    let mut settings = match std::fs::read(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => AppSettings::default(),
        Err(e) => {
            warnings.push(format!(
                "Cannot read {}: {e}; using default settings",
                path.display()
            ));
            AppSettings::default()
        }
        Ok(bytes) => match serde_json::from_slice::<serde_json::Value>(&bytes) {
            Ok(serde_json::Value::Object(map)) => parse_sections(map, &mut warnings),
            Ok(_) | Err(_) => {
                let mut w = format!("{} is corrupt; using default settings", path.display());
                match backup_corrupt_file(&path) {
                    Ok(b) => w.push_str(&format!(" (the old file was saved as {})", b.display())),
                    Err(e) => w.push_str(&format!(" (backup failed: {e})")),
                }
                warnings.push(w);
                AppSettings::default()
            }
        },
    };
    let (keymap, kw) = keymap::load_config(paths);
    settings.keymap = keymap;
    warnings.extend(kw);
    for w in &warnings {
        tracing::warn!("{w}");
    }
    (settings, warnings)
}

/// Typed parse with per-section fallback.
fn parse_sections(
    map: serde_json::Map<String, serde_json::Value>,
    warnings: &mut Vec<String>,
) -> AppSettings {
    let value = serde_json::Value::Object(map);
    if let Ok(s) = serde_json::from_value::<AppSettings>(value.clone()) {
        return s;
    }
    let serde_json::Value::Object(map) = value else {
        return AppSettings::default();
    };
    let base = serde_json::to_value(AppSettings::default()).unwrap_or(serde_json::Value::Null);
    let mut kept = serde_json::Map::new();
    for (key, section) in map {
        let mut probe = base.clone();
        if let Some(obj) = probe.as_object_mut() {
            obj.insert(key.clone(), section.clone());
        }
        if serde_json::from_value::<AppSettings>(probe).is_ok() {
            kept.insert(key, section);
        } else {
            warnings.push(format!(
                "settings: section '{key}' is invalid and was reset to defaults"
            ));
        }
    }
    serde_json::from_value(serde_json::Value::Object(kept)).unwrap_or_default()
}

/// Writes `settings.json` and `keymap.json` atomically (mode 0600).
pub fn save(paths: &AppPaths, settings: &AppSettings) -> DbResult<()> {
    let mut s = settings.clone();
    s.version = SETTINGS_VERSION;
    write_json(&paths.settings_file(), &s)?;
    keymap::save_config(paths, &settings.keymap)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keymap::KeymapPreset;

    fn paths() -> (tempfile::TempDir, AppPaths) {
        let dir = tempfile::tempdir().unwrap();
        let p = AppPaths::with_root(dir.path());
        (dir, p)
    }

    #[test]
    fn defaults_when_missing() {
        let (_d, p) = paths();
        let (s, w) = load_with_warnings(&p);
        assert!(w.is_empty());
        assert_eq!(s, AppSettings::default());
        assert_eq!(s.results.fetch_size, 200);
        assert_eq!(s.ai.model, "claude-opus-5-5");
        assert!(!s.ai.enabled);
        assert!(!s.ai.send_sample_data);
        assert_eq!(s.security.secret_backend, SecretBackend::Keychain);
    }

    #[test]
    fn roundtrip_including_keymap() {
        let (_d, p) = paths();
        let mut s = AppSettings::default();
        s.general.theme = Theme::Dark;
        s.general.locale = Some(Locale::Id);
        s.editor.format.comma_first = true;
        s.results.fetch_size = 500;
        s.keymap.preset = KeymapPreset::Dbeaver;
        s.keymap
            .overrides
            .insert("editor.format_sql".into(), vec!["Cmd+Alt+L".into()]);
        save(&p, &s).unwrap();
        let text = std::fs::read_to_string(p.settings_file()).unwrap();
        assert!(!text.contains("keymap"), "keymap must live in keymap.json");
        assert!(p.keymap_file().exists());
        let back = load(&p);
        assert_eq!(back, s);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(p.settings_file())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
    }

    #[test]
    fn old_and_partial_files_keep_loading() {
        let (_d, p) = paths();
        std::fs::create_dir_all(p.config_dir()).unwrap();
        std::fs::write(
            p.settings_file(),
            r#"{"editor": {"font_size": 15.0, "unknown_future_option": true},
                "general": {"theme": "solarized"},
                "results": {"layout": "diagonal", "fetch_size": 1000},
                "brand_new_section": {"x": 1}}"#,
        )
        .unwrap();
        let (s, w) = load_with_warnings(&p);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(s.editor.font_size, 15.0);
        assert_eq!(s.editor.tab_size, 4);
        assert_eq!(s.general.theme, Theme::System);
        assert_eq!(s.results.layout, ResultsLayout::Bottom);
        assert_eq!(s.results.fetch_size, 1000);
    }

    #[test]
    fn invalid_section_is_reset_alone() {
        let (_d, p) = paths();
        std::fs::create_dir_all(p.config_dir()).unwrap();
        std::fs::write(
            p.settings_file(),
            r#"{"editor": {"font_size": "huge"}, "results": {"fetch_size": 42}}"#,
        )
        .unwrap();
        let (s, w) = load_with_warnings(&p);
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("editor"));
        assert_eq!(s.editor, EditorSettings::default());
        assert_eq!(s.results.fetch_size, 42);
        assert!(p.settings_file().exists(), "valid JSON is not moved aside");
    }

    #[test]
    fn corrupt_file_is_backed_up() {
        let (_d, p) = paths();
        std::fs::create_dir_all(p.config_dir()).unwrap();
        std::fs::write(p.settings_file(), "{ this is not json").unwrap();
        let (s, w) = load_with_warnings(&p);
        assert_eq!(s, AppSettings::default());
        assert_eq!(w.len(), 1);
        assert!(!p.settings_file().exists());
        let backups: Vec<_> = std::fs::read_dir(p.config_dir())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with("settings.json.corrupt-")
            })
            .collect();
        assert_eq!(backups.len(), 1);
    }

    #[test]
    fn helpers() {
        let s = AppSettings::default();
        assert_eq!(s.results.store_budget_bytes(), 48 * 1024 * 1024);
        assert_eq!(s.ai.effort.as_str(), "high");
        assert_eq!(AiProvider::OpenAiCompatible.id(), "openai-compatible");
        assert!(
            s.toolbar
                .visible_actions()
                .contains(&crate::actions::Action::ExecuteStatement)
        );
        let r = s.history.retention();
        assert_eq!(r.max_entries, Some(50_000));
        assert_eq!(r.max_age_days, Some(90));
        let json = serde_json::to_value(&s).unwrap();
        assert_eq!(json["ai"]["effort"], "high");
        assert_eq!(json["general"]["theme"], "system");
        let _ = s.general.effective_locale();
    }
}
