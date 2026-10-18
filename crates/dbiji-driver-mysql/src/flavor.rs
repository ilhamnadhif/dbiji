//! Server flavor (MySQL or MariaDB) and version dependent feature checks.

use dbiji_core::DbKind;

/// The two SQL dialect families served by this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Flavor {
    /// Oracle MySQL (and compatible servers such as Percona Server).
    MySql,
    /// MariaDB.
    MariaDb,
}

impl Flavor {
    /// Flavor configured for a [`DbKind`] (`MariaDb` for [`DbKind::MariaDb`], MySQL otherwise).
    pub fn from_kind(kind: DbKind) -> Flavor {
        match kind {
            DbKind::MariaDb => Flavor::MariaDb,
            _ => Flavor::MySql,
        }
    }

    /// The matching [`DbKind`].
    pub fn kind(self) -> DbKind {
        match self {
            Flavor::MySql => DbKind::MySql,
            Flavor::MariaDb => DbKind::MariaDb,
        }
    }

    /// Product name for display.
    pub fn product(self) -> &'static str {
        match self {
            Flavor::MySql => "MySQL",
            Flavor::MariaDb => "MariaDB",
        }
    }

    /// Detect the flavor from a `VERSION()` / handshake version string.
    pub fn detect(version: &str) -> Flavor {
        if version.to_ascii_lowercase().contains("mariadb") {
            Flavor::MariaDb
        } else {
            Flavor::MySql
        }
    }
}

/// Parse `major.minor.patch` from a server version string, skipping the
/// `5.5.5-` replication prefix MariaDB puts in front of its real version in
/// the protocol handshake.
pub fn parse_server_version(version: &str) -> (u32, u32, u32) {
    let v = version.trim();
    let v = if v.starts_with("5.5.5-") && v.to_ascii_lowercase().contains("mariadb") {
        &v[6..]
    } else {
        v
    };
    dbiji_core::driver::parse_version_number(v)
}

/// What the driver knows about the connected server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServerProfile {
    /// MySQL or MariaDB.
    pub flavor: Flavor,
    /// `(major, minor, patch)`; `(0, 0, 0)` when unknown.
    pub version: (u32, u32, u32),
    /// `@@sql_mode` (upper case) at connect time.
    pub sql_mode: String,
    /// `@@lower_case_table_names`.
    pub lower_case_table_names: u32,
}

impl ServerProfile {
    /// Profile for a flavor without a known server version. Feature checks
    /// answer conservatively (the most portable syntax is used).
    pub fn unknown(flavor: Flavor) -> Self {
        ServerProfile {
            flavor,
            version: (0, 0, 0),
            sql_mode: String::new(),
            lower_case_table_names: 0,
        }
    }

    /// Profile for a flavor and version (default `sql_mode`).
    pub fn new(flavor: Flavor, version: (u32, u32, u32)) -> Self {
        ServerProfile {
            flavor,
            version,
            sql_mode: String::new(),
            lower_case_table_names: 0,
        }
    }

    /// The server is MariaDB.
    pub fn is_mariadb(&self) -> bool {
        self.flavor == Flavor::MariaDb
    }

    /// The server is MySQL (or compatible).
    pub fn is_mysql(&self) -> bool {
        self.flavor == Flavor::MySql
    }

    /// The version is known.
    pub fn known(&self) -> bool {
        self.version != (0, 0, 0)
    }

    /// `version >= (major, minor, patch)`.
    pub fn at_least(&self, major: u32, minor: u32, patch: u32) -> bool {
        self.version >= (major, minor, patch)
    }

    fn mysql(&self, major: u32, minor: u32, patch: u32) -> bool {
        self.is_mysql() && self.at_least(major, minor, patch)
    }

    fn mariadb(&self, major: u32, minor: u32, patch: u32) -> bool {
        self.is_mariadb() && self.at_least(major, minor, patch)
    }

    /// `information_schema.CHECK_CONSTRAINTS` exists and CHECK is enforced.
    pub fn has_check_constraints(&self) -> bool {
        self.mysql(8, 0, 16) || self.mariadb(10, 2, 1)
    }

    /// `CHECK_CONSTRAINTS` has a `TABLE_NAME` column (MariaDB).
    pub fn check_constraints_have_table(&self) -> bool {
        self.mariadb(10, 2, 1)
    }

    /// Functional key parts (`STATISTICS.EXPRESSION`).
    pub fn has_functional_indexes(&self) -> bool {
        self.mysql(8, 0, 13)
    }

    /// `STATISTICS.IS_VISIBLE` (MySQL 8 invisible indexes).
    pub fn has_invisible_indexes(&self) -> bool {
        self.mysql(8, 0, 0)
    }

    /// `STATISTICS.IGNORED` (MariaDB 10.6 ignored indexes).
    pub fn has_ignored_indexes(&self) -> bool {
        self.mariadb(10, 6, 0)
    }

    /// `EXPLAIN FORMAT=JSON` (MySQL 5.6.5+, MariaDB 10.1+).
    pub fn has_explain_json(&self) -> bool {
        !self.known() || self.mysql(5, 6, 5) || self.mariadb(10, 1, 0)
    }

    /// `EXPLAIN FORMAT=TREE` (MySQL 8.0.16+).
    pub fn has_explain_tree(&self) -> bool {
        self.mysql(8, 0, 16)
    }

    /// `EXPLAIN ANALYZE` (MySQL 8.0.18+) or `ANALYZE FORMAT=JSON` (MariaDB 10.1+).
    pub fn has_explain_analyze(&self) -> bool {
        self.mysql(8, 0, 18) || self.mariadb(10, 1, 0)
    }

    /// `ALTER TABLE ... RENAME COLUMN` (MySQL 8.0+, MariaDB 10.5.2+).
    pub fn has_rename_column(&self) -> bool {
        self.mysql(8, 0, 0) || self.mariadb(10, 5, 2)
    }

    /// Sequences (MariaDB 10.3+).
    pub fn has_sequences(&self) -> bool {
        self.mariadb(10, 3, 0)
    }

    /// `information_schema.SEQUENCES` (MariaDB 11.? has it; probe on failure).
    pub fn has_sequences_view(&self) -> bool {
        self.mariadb(10, 6, 0)
    }

    /// `information_schema.VIEW_TABLE_USAGE` (MySQL 8.0.13+).
    pub fn has_view_table_usage(&self) -> bool {
        self.mysql(8, 0, 13)
    }

    /// `performance_schema.data_locks` / `data_lock_waits` (MySQL 8.0.1+).
    pub fn has_data_locks(&self) -> bool {
        self.mysql(8, 0, 1)
    }

    /// `information_schema.INNODB_LOCKS` / `INNODB_LOCK_WAITS` (MySQL 5.x, MariaDB).
    pub fn has_innodb_locks_views(&self) -> bool {
        self.is_mariadb() || (self.is_mysql() && !self.at_least(8, 0, 0))
    }

    /// Row alias in `INSERT ... AS new ON DUPLICATE KEY UPDATE c = new.c` (MySQL 8.0.19+).
    pub fn has_upsert_row_alias(&self) -> bool {
        self.mysql(8, 0, 19)
    }

    /// `INSERT ... RETURNING` (MariaDB 10.5+).
    pub fn has_returning(&self) -> bool {
        self.mariadb(10, 5, 0)
    }

    /// MySQL 8 caches `information_schema.TABLES` statistics
    /// (`information_schema_stats_expiry`).
    pub fn has_stats_expiry(&self) -> bool {
        self.mysql(8, 0, 3)
    }

    /// `SCHEMATA.SCHEMA_COMMENT` (MariaDB 10.5+).
    pub fn has_schema_comment(&self) -> bool {
        self.mariadb(10, 5, 0)
    }

    /// `DROP CHECK name` (MySQL) vs `DROP CONSTRAINT name` (MariaDB).
    pub fn drop_check_keyword(&self) -> &'static str {
        if self.is_mariadb() {
            "CONSTRAINT"
        } else {
            "CHECK"
        }
    }

    /// MariaDB 10.2.7+ reports `COLUMN_DEFAULT` as SQL expressions.
    pub fn defaults_are_expressions(&self) -> bool {
        self.mariadb(10, 2, 7)
    }

    /// `CREATE OR REPLACE` for routines, triggers and events (MariaDB 10.1.4+).
    pub fn has_create_or_replace_routines(&self) -> bool {
        self.mariadb(10, 1, 4)
    }

    /// Descending index key parts are honored (MySQL 8.0, MariaDB 10.8).
    pub fn has_descending_indexes(&self) -> bool {
        self.mysql(8, 0, 0) || self.mariadb(10, 8, 0)
    }

    /// `sql_mode` contains `ANSI_QUOTES` (double quotes delimit identifiers).
    pub fn ansi_quotes(&self) -> bool {
        self.sql_mode_has("ANSI_QUOTES")
    }

    /// `sql_mode` contains `NO_BACKSLASH_ESCAPES`.
    pub fn no_backslash_escapes(&self) -> bool {
        self.sql_mode_has("NO_BACKSLASH_ESCAPES")
    }

    fn sql_mode_has(&self, flag: &str) -> bool {
        self.sql_mode
            .split(',')
            .any(|m| m.trim().eq_ignore_ascii_case(flag))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_flavor() {
        assert_eq!(Flavor::detect("8.0.46"), Flavor::MySql);
        assert_eq!(Flavor::detect("11.8.9-MariaDB-ubu2404"), Flavor::MariaDb);
        assert_eq!(Flavor::detect("5.5.5-10.11.6-MariaDB-log"), Flavor::MariaDb);
    }

    #[test]
    fn versions() {
        assert_eq!(parse_server_version("5.5.5-10.11.6-MariaDB"), (10, 11, 6));
        assert_eq!(parse_server_version("8.0.46"), (8, 0, 46));
        assert_eq!(parse_server_version("5.7.44-log"), (5, 7, 44));
    }

    #[test]
    fn features() {
        let my57 = ServerProfile::new(Flavor::MySql, (5, 7, 44));
        let my80 = ServerProfile::new(Flavor::MySql, (8, 0, 46));
        let ma = ServerProfile::new(Flavor::MariaDb, (11, 8, 9));
        assert!(!my57.has_check_constraints());
        assert!(my80.has_check_constraints());
        assert!(ma.has_check_constraints() && ma.check_constraints_have_table());
        assert!(my80.has_explain_tree() && !ma.has_explain_tree());
        assert!(
            my80.has_explain_analyze() && ma.has_explain_analyze() && !my57.has_explain_analyze()
        );
        assert!(!my57.has_rename_column() && my80.has_rename_column() && ma.has_rename_column());
        assert!(ma.has_sequences() && !my80.has_sequences());
        assert!(
            my57.has_innodb_locks_views()
                && !my80.has_innodb_locks_views()
                && ma.has_innodb_locks_views()
        );
        assert!(my80.has_upsert_row_alias() && !ma.has_upsert_row_alias());
        let mut p = ServerProfile::new(Flavor::MySql, (8, 0, 0));
        p.sql_mode = "ANSI_QUOTES,NO_BACKSLASH_ESCAPES".into();
        assert!(p.ansi_quotes() && p.no_backslash_escapes());
    }
}
