//! Catalog folders of SQLite (see `dbiji_core::catalog`): the database
//! settings (pragmas) and the library's compile options.

use dbiji_core::catalog::{
    CatalogFolder as F, CatalogGroup as G, CatalogIcon as I, CatalogScope as S,
};

const PRAGMAS: &[&str] = &[
    "application_id",
    "auto_vacuum",
    "busy_timeout",
    "cache_size",
    "encoding",
    "foreign_keys",
    "freelist_count",
    "journal_mode",
    "page_count",
    "page_size",
    "recursive_triggers",
    "secure_delete",
    "synchronous",
    "temp_store",
    "user_version",
];

pub fn folders() -> Vec<F> {
    // A scalar subquery: the column of `pragma_x` is not always named `x`.
    let settings = PRAGMAS
        .iter()
        .map(|p| {
            format!("SELECT '{p}' AS name, CAST((SELECT * FROM pragma_{p}) AS TEXT) AS detail")
        })
        .collect::<Vec<_>>()
        .join("\nUNION ALL ");
    vec![
        F::new(
            "sqlite.settings",
            "Settings",
            G::Administer,
            S::Server,
            I::Settings,
            settings,
        )
        .table_only(),
        F::new(
            "sqlite.compile_options",
            "Compile Options",
            G::SystemInfo,
            S::Server,
            I::Gauge,
            "SELECT compile_options AS name FROM pragma_compile_options",
        )
        .table_only(),
    ]
}
