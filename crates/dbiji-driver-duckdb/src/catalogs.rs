//! Catalog folders of DuckDB (see `dbiji_core::catalog`): settings and
//! extensions.

use dbiji_core::catalog::{
    CatalogFolder as F, CatalogGroup as G, CatalogIcon as I, CatalogScope as S,
};

pub fn folders() -> Vec<F> {
    vec![
        F::new("duckdb.settings", "Settings", G::Administer, S::Server, I::Settings, "SELECT name, value AS detail, value, description, input_type FROM duckdb_settings() ORDER BY name")
            .table_only(),
        F::new(
            "duckdb.extensions",
            "Extensions",
            G::Administer,
            S::Server,
            I::Puzzle,
            "SELECT extension_name AS name, CASE WHEN loaded THEN 'loaded' WHEN installed THEN 'installed' ELSE 'available' END AS detail,
  installed, loaded, extension_version, description FROM duckdb_extensions() ORDER BY 1",
        ),
    ]
}
