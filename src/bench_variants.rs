//! Dispatch a benchmark over selected variants and aggregate refusals.

use crate::{error::DevError, output::console};

/// Run a closure for each variant, collecting failures instead of aborting.
///
/// Each variant runs independently - failure of one does not skip the rest.
/// Each failure is printed live with its cause. On completion, returns
/// `Ok(())` if all succeeded, or a bare summary error naming which variants
/// failed.
///
/// An empty `variants` list is an error, not a vacuous success. Callers build
/// the list by filtering a fixed set against a user selector (`--query NAME`,
/// `--mode NAME`, ...), so an empty list means the selector matched nothing -
/// a typo. Returning `Ok` there recorded no rows and exited 0, which reads
/// exactly like a successful bench.
///
/// Usage:
/// ```ignore
/// run_variants("mode", &["sequential", "parallel", "pipelined"], |variant| {
///     // set up config using variant name...
///     harness.run_external(&config, binary, &args, project_root)
/// })?;
/// ```
pub fn run_variants<F>(label: &str, variants: &[&str], mut run_one: F) -> Result<(), DevError>
where
    F: FnMut(&str) -> Result<(), DevError>,
{
    if variants.is_empty() {
        return Err(DevError::Refused(format!(
            "no {label} variants selected - nothing to benchmark (check the {label} name)"
        )));
    }

    let mut failures: Vec<&str> = Vec::new();

    for &variant in variants {
        console::bench_msg(&format!("{label}: {variant}"));
        match run_one(variant) {
            Ok(()) => {}
            // A cooperative shutdown ends the whole run, not one variant.
            Err(e @ DevError::Interrupted) => return Err(e),
            Err(e) => {
                console::error(&format!("{variant} failed: {e}"));
                failures.push(variant);
            }
        }
    }

    if failures.is_empty() {
        Ok(())
    } else {
        Err(variants_failed(&failures, variants.len()))
    }
}

/// The summary error [`run_variants`] ends with. Each failure's cause was
/// already printed live as `<variant> failed: ...`, so this names the variants
/// only and renders bare (`Reported`) rather than repeating the causes under a
/// `verify:` prefix.
pub(crate) fn variants_failed(failed: &[&str], total: usize) -> DevError {
    DevError::Reported(format!(
        "{} of {} variants failed: {}",
        failed.len(),
        total,
        failed.join(", "),
    ))
}
