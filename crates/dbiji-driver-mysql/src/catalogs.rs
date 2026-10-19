//! Catalog folders of MySQL and MariaDB (see `dbiji_core::catalog`): users
//! with their grants, and the System Info folders DBeaver shows — status and
//! system variables, storage engines, character sets with their collations,
//! plugins.

use dbiji_core::catalog::{
    CatalogFolder as F, CatalogGroup as G, CatalogIcon as I, CatalogScope as S,
};

pub fn folders() -> Vec<F> {
    vec![
        F::new(
            "my.users",
            "Users",
            G::Security,
            S::Server,
            I::User,
            "SELECT CONCAT(QUOTE(User), '@', QUOTE(Host)) AS name, plugin AS detail, User AS user, Host AS host, plugin AS authentication
FROM mysql.user ORDER BY User, Host",
        )
        .ddl("SHOW CREATE USER {name_raw}")
        .ddl("SHOW GRANTS FOR {name_raw}"),
        F::new("my.session_status", "Session Status", G::SystemInfo, S::Server, I::Gauge, "SHOW SESSION STATUS").table_only(),
        F::new("my.global_status", "Global Status", G::SystemInfo, S::Server, I::Gauge, "SHOW GLOBAL STATUS").table_only(),
        F::new("my.session_variables", "Session Variables", G::SystemInfo, S::Server, I::Settings, "SHOW SESSION VARIABLES").table_only(),
        F::new("my.global_variables", "Global Variables", G::SystemInfo, S::Server, I::Settings, "SHOW GLOBAL VARIABLES").table_only(),
        F::new(
            "my.engines",
            "Engines",
            G::SystemInfo,
            S::Server,
            I::Engine,
            "SELECT ENGINE AS name, SUPPORT AS detail, COMMENT AS comment, TRANSACTIONS AS transactions, XA AS xa, SAVEPOINTS AS savepoints
FROM information_schema.ENGINES ORDER BY ENGINE",
        ),
        F::new(
            "my.charsets",
            "Charsets",
            G::SystemInfo,
            S::Server,
            I::Text,
            "SELECT CHARACTER_SET_NAME AS name, DESCRIPTION AS detail, DEFAULT_COLLATE_NAME AS default_collation, MAXLEN AS max_length
FROM information_schema.CHARACTER_SETS ORDER BY CHARACTER_SET_NAME",
        )
        .children(&["my.collations"]),
        F::new(
            "my.collations",
            "Collations",
            G::None,
            S::Child,
            I::Text,
            "SELECT COLLATION_NAME AS name, CASE WHEN IS_DEFAULT = 'Yes' THEN 'default' END AS detail, ID AS id, IS_COMPILED AS compiled, SORTLEN AS sort_length
FROM information_schema.COLLATIONS WHERE CHARACTER_SET_NAME = {parent} ORDER BY COLLATION_NAME",
        ),
        F::new(
            "my.plugins",
            "Plugins",
            G::SystemInfo,
            S::Server,
            I::Puzzle,
            "SELECT PLUGIN_NAME AS name, PLUGIN_STATUS AS detail, PLUGIN_TYPE AS type, PLUGIN_VERSION AS version,
  PLUGIN_LIBRARY AS library, LOAD_OPTION AS load_option
FROM information_schema.PLUGINS ORDER BY PLUGIN_NAME",
        ),
    ]
}
