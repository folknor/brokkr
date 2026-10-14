//! Service command shapes, split by how scripts are selected and run.

mod list;
mod single;
mod suite;

pub use list::service_list;
pub(crate) use single::service_test;
pub(crate) use suite::service_suite;
