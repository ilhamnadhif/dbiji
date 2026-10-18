//! Registration of the database drivers compiled into this binary.

use dbiji_core::registry::DriverRegistry;

/// Register every built-in driver.
pub fn register_all(reg: &DriverRegistry) {
    let _ = reg;
}
