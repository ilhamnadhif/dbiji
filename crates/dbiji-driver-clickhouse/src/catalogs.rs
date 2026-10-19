//! Catalog folders of ClickHouse (see `dbiji_core::catalog`): users, roles,
//! settings, disks, clusters, metrics and events.

use dbiji_core::catalog::{CatalogFolder as F, CatalogGroup as G, CatalogIcon as I, CatalogScope as S};

pub fn folders() -> Vec<F> {
    vec![
        F::new("ch.users", "Users", G::Security, S::Server, I::User, "SELECT name, storage AS detail, storage FROM system.users ORDER BY name")
            .ddl("SHOW CREATE USER {name_ident}")
            .ddl("SHOW GRANTS FOR {name_ident}"),
        F::new("ch.roles", "Roles", G::Security, S::Server, I::Users, "SELECT name, storage AS detail, storage FROM system.roles ORDER BY name")
            .ddl("SHOW CREATE ROLE {name_ident}")
            .ddl("SHOW GRANTS FOR {name_ident}"),
        F::new(
            "ch.disks",
            "Disks",
            G::Storage,
            S::Server,
            I::Storage,
            "SELECT name, formatReadableSize(free_space) || ' free' AS detail, path, formatReadableSize(free_space) AS free,
  formatReadableSize(total_space) AS total FROM system.disks ORDER BY name",
        ),
        F::new("ch.clusters", "Clusters", G::Administer, S::Server, I::Server, "SELECT DISTINCT cluster AS name FROM system.clusters ORDER BY name"),
        F::new(
            "ch.settings",
            "Settings",
            G::Administer,
            S::Server,
            I::Settings,
            "SELECT name, value AS detail, value, changed, type, description FROM system.settings ORDER BY name",
        )
        .table_only(),
        F::new("ch.metrics", "Metrics", G::SystemInfo, S::Server, I::Gauge, "SELECT metric AS name, toString(value) AS detail, description FROM system.metrics ORDER BY metric")
            .table_only(),
        F::new("ch.events", "Events", G::SystemInfo, S::Server, I::Gauge, "SELECT event AS name, toString(value) AS detail, description FROM system.events ORDER BY event")
            .table_only(),
    ]
}
