//! Catalog folders of SQL Server (see `dbiji_core::catalog`): logins and the
//! server configuration, database users, roles and DDL triggers, and the
//! extended properties of tables.

use dbiji_core::catalog::{CatalogFolder as F, CatalogGroup as G, CatalogIcon as I, CatalogScope as S};

pub fn folders() -> Vec<F> {
    vec![
        F::new(
            "mssql.logins",
            "Logins",
            G::Security,
            S::Server,
            I::User,
            "SELECT name, type_desc AS detail, type_desc AS type, is_disabled, default_database_name AS default_database,
  default_language_name AS default_language, create_date, modify_date
FROM sys.server_principals WHERE type IN ('S', 'U', 'G', 'E', 'X') ORDER BY name",
        )
        .ddl(
            "SELECT 'CREATE LOGIN ' + QUOTENAME(name) + CASE WHEN type IN ('U', 'G') THEN ' FROM WINDOWS'
  WHEN type IN ('E', 'X') THEN ' FROM EXTERNAL PROVIDER'
  ELSE ' WITH PASSWORD = N''<password>''' + ISNULL(', DEFAULT_DATABASE = ' + QUOTENAME(default_database_name), '') END
FROM sys.server_principals WHERE name = {name}",
        ),
        F::new(
            "mssql.configuration",
            "Server Configuration",
            G::Administer,
            S::Server,
            I::Settings,
            "SELECT name, CAST(value_in_use AS nvarchar(64)) AS detail, CAST(value AS nvarchar(64)) AS configured,
  CAST(minimum AS nvarchar(64)) AS minimum, CAST(maximum AS nvarchar(64)) AS maximum, description, is_dynamic, is_advanced
FROM sys.configurations ORDER BY name",
        )
        .table_only(),
        F::new(
            "mssql.users",
            "Users",
            G::None,
            S::Database,
            I::User,
            "SELECT name, type_desc AS detail, default_schema_name AS default_schema, create_date, modify_date
FROM sys.database_principals WHERE type IN ('S', 'U', 'G', 'E', 'X') ORDER BY name",
        ),
        F::new(
            "mssql.roles",
            "Database Roles",
            G::None,
            S::Database,
            I::Users,
            "SELECT name, CASE WHEN is_fixed_role = 1 THEN 'fixed' END AS detail, create_date, modify_date
FROM sys.database_principals WHERE type = 'R' ORDER BY name",
        )
        .children(&["mssql.role_members"]),
        F::new(
            "mssql.role_members",
            "Members",
            G::None,
            S::Child,
            I::User,
            "SELECT m.name, m.type_desc AS detail FROM sys.database_role_members rm
JOIN sys.database_principals r ON r.principal_id = rm.role_principal_id
JOIN sys.database_principals m ON m.principal_id = rm.member_principal_id
WHERE r.name = {parent} ORDER BY m.name",
        ),
        F::new(
            "mssql.database_triggers",
            "Database Triggers",
            G::None,
            S::Database,
            I::Trigger,
            "SELECT name, CASE WHEN is_disabled = 1 THEN 'disabled' END AS detail, create_date, modify_date
FROM sys.triggers WHERE parent_class = 0 ORDER BY name",
        )
        .ddl("SELECT OBJECT_DEFINITION(object_id) FROM sys.triggers WHERE parent_class = 0 AND name = {name}"),
        F::new(
            "mssql.extended_properties",
            "Extended Properties",
            G::None,
            S::Table,
            I::Text,
            "SELECT ep.name, CAST(ep.value AS nvarchar(4000)) AS detail FROM sys.extended_properties ep
WHERE ep.class = 1 AND ep.minor_id = 0 AND ep.major_id = OBJECT_ID(QUOTENAME({schema}) + '.' + QUOTENAME({table}))
ORDER BY ep.name",
        ),
    ]
}
