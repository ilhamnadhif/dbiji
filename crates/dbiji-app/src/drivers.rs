//! Registration of the database drivers compiled into this binary.

use dbiji_core::registry::DriverRegistry;
use std::sync::Arc;

/// Register every built-in driver.
pub fn register_all(reg: &mut DriverRegistry) {
    dbiji_driver_postgres::register(reg);
    dbiji_driver_mysql::register(reg);
    reg.register(Arc::new(dbiji_driver_mssql::SqlServerDriver::new()));
    reg.register(Arc::new(dbiji_driver_clickhouse::ClickHouseDriver::new()));
    dbiji_driver_oracle::register(reg);
    dbiji_driver_cassandra::register(reg);
    reg.register(Arc::new(dbiji_driver_sqlite::SqliteDriver::new()));
}
