//! Registry of available drivers.

use crate::config::DbKind;
use crate::dialect::{Dialect, GenericDialect};
use crate::driver::Driver;
use std::collections::BTreeMap;
use std::sync::Arc;

#[derive(Clone, Default)]
pub struct DriverRegistry {
    drivers: BTreeMap<DbKind, Arc<dyn Driver>>,
}

impl DriverRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a driver under its own kind.
    pub fn register(&mut self, driver: Arc<dyn Driver>) {
        self.drivers.insert(driver.kind(), driver);
    }

    /// Register a driver under an additional kind (e.g. the MySQL driver for MariaDB).
    pub fn register_as(&mut self, kind: DbKind, driver: Arc<dyn Driver>) {
        self.drivers.insert(kind, driver);
    }

    pub fn get(&self, kind: DbKind) -> Option<Arc<dyn Driver>> {
        self.drivers.get(&kind).cloned()
    }

    pub fn kinds(&self) -> Vec<DbKind> {
        self.drivers.keys().copied().collect()
    }

    pub fn drivers(&self) -> impl Iterator<Item = (DbKind, &Arc<dyn Driver>)> {
        self.drivers.iter().map(|(k, d)| (*k, d))
    }

    /// Dialect for `kind`, falling back to ANSI SQL.
    pub fn dialect(&self, kind: DbKind) -> Arc<dyn Dialect> {
        self.get(kind).map(|d| d.dialect()).unwrap_or_else(|| Arc::new(GenericDialect::new()))
    }
}
