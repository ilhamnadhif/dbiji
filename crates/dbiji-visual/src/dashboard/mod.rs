//! Dashboards: the server health dashboard (Administration) fed with
//! `Session::server_metrics` samples, and custom dashboards of SQL charts.

mod custom;
mod history;
mod view;

pub use custom::{
    CustomDashboard, CustomDashboardEvent, CustomDashboardState, DashboardItem, ItemResult, show_custom_dashboard,
};
pub use history::{DEFAULT_CAPACITY, DashboardState, MetricHistory, WindowStats};
pub use view::{DashboardLayout, DashboardResponse, WindowLength, show_dashboard};
