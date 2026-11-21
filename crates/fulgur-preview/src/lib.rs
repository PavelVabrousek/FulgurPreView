//! FulgurPreView desktop application library and automated M1 verification suite.

pub mod app;
pub mod cache;
pub mod crash;
pub mod directory;
pub mod field_map;
pub mod jobs;
pub mod slider_scale;
pub mod small_body_view;
pub mod smoke;
pub mod viewer;
pub mod watch;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
