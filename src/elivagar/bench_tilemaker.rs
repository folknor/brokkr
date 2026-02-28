//! Benchmark: Tilemaker Shortbread tilegen for comparison.
//!
//! Replaces `bench-tilemaker.sh`. Currently a stub -- Tilemaker requires
//! auto-download of the source, shortbread-tilemaker config, and ocean
//! shapefiles in 4326 projection with ogr2ogr reprojection. This will be
//! implemented in a future iteration.

use crate::error::DevError;

// ---------------------------------------------------------------------------
// Public entry point
// ---------------------------------------------------------------------------

pub fn run() -> Result<(), DevError> {
    Err(DevError::Config(
        "tilemaker benchmark not yet implemented in dev tool".into(),
    ))
}
