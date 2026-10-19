//! Persistent configuration of DBiji: application paths, preferences,
//! keyboard shortcuts, the connection store, secret storage (OS keychain or
//! an encrypted vault), importers (DBeaver, pgpass, URLs), the query history,
//! projects (scripts, bookmarks, diagrams, virtual models) and SQL templates.
//!
//! Security rules followed throughout:
//! * secrets are [`dbiji_core::Secret`]s and never written to configuration
//!   files or logs; they live in a [`secrets::SecretStore`];
//! * configuration and vault files are written atomically with mode 0600 in
//!   directories with mode 0700 ([`paths::write_secure`]);
//! * key material is zeroized after use.

// `DbError` (the shared error type of dbiji-core) is ~144 bytes; every
// fallible function returns it, so the lint is not actionable in this crate.
#![allow(clippy::result_large_err)]
#![warn(missing_docs)]

pub mod actions;
pub mod connections;
pub mod history;
pub mod import;
pub mod keymap;
pub mod paths;
pub mod project;
pub mod secrets;
pub mod settings;
pub mod templates;

pub use actions::{Action, ActionCategory, ActionContext};
pub use connections::{ConnectionStore, EffectiveNetwork, profile_secret_key, validate};
pub use history::{HistoryEntry, HistoryQuery, HistoryStore, QueryStatus, RetentionPolicy};
pub use import::{ImportResult, import_pgpass, parse_connection_url, to_url};
pub use keymap::{Keymap, KeymapConfig, KeymapPreset, Shortcut};
pub use paths::{AppPaths, read_json, write_json, write_secure};
pub use project::{
    Bookmark, Project, ProjectManager, ScriptMeta, SessionState, SessionTab, VirtualModel,
    VirtualTable,
};
pub use secrets::{
    BundledStore, KeychainStore, MemoryStore, SecretStore, SecretsManager, VaultStore, ai_key,
};
pub use settings::{AppSettings, SecretBackend};
pub use templates::{SqlTemplate, TemplateLibrary, expand};
