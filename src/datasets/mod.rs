//! Dataset selection, input validation and geometry.

mod bbox;
mod element_ids;
pub(crate) mod resolve;
pub(crate) use bbox::Bbox;
pub(crate) use element_ids::ElementIdList;
pub(crate) mod error;
pub(crate) mod status;
