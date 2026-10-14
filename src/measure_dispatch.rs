//! Shared mode dispatch for the small benchmark adapters.

use crate::error::DevError;
use crate::measure::MeasureRequest;
use crate::output;

/// Run the selected profiling path, or the adapter's ordinary measured path.
/// Projects keep their distinct build and harness contracts in their own
/// modules; this owns the shared mode selection and profiler narration.
pub(crate) fn dispatch_profiled<T>(
    request: &MeasureRequest<'_>,
    project: &str,
    subject: &str,
    profiled: impl FnOnce() -> Result<T, DevError>,
    measured: impl FnOnce() -> Result<T, DevError>,
) -> Result<T, DevError> {
    if let Some(mode) = request.mode.profiled() {
        output::detail(&format!("{project} {}: {subject}", mode.feature()));
        mode.print_note();
        profiled()
    } else {
        measured()
    }
}
