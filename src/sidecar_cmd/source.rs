//! Where a view's evidence comes from: a typed run's rows in sidecar.db, read
//! by the queries it always used, or one child of a script-bench run's
//! iterations, converted from the stored document. The views are written
//! once over it.

use super::bench::BenchRun;
use crate::db;
use crate::error::DevError;
use crate::sidecar::{CallSpan, Counter, Marker, Sample, StoredSinkEnd};
use crate::sidecar_fmt::Process;

pub(super) enum Source<'a> {
    Typed {
        store: &'a db::RunStore,
        id: db::RunId,
    },
    Bench(Box<BenchRun>),
}

impl Source<'_> {
    /// The provenance header, on stderr.
    pub(super) fn print_info(&self) -> Result<(), DevError> {
        match self {
            Self::Typed { store, id } => crate::sidecar_fmt::print_run_info(store, id),
            Self::Bench(run) => {
                run.print_header();
                Ok(())
            }
        }
    }

    /// `--run`'s selection, with the stored capture's warnings and which run
    /// is shown. A bench run's selection was judged when it resolved.
    pub(super) fn run_filter(&self, run_arg: Option<&str>) -> Result<Option<usize>, DevError> {
        match self {
            Self::Typed { store, id } => super::resolve_run_filter(store, id, run_arg),
            Self::Bench(run) => {
                run.warn_and_show();
                Ok(run.filter())
            }
        }
    }

    /// The samples, each with the iteration it belongs to for a bench child
    /// (`None` for a typed run, whose sample JSON never named its run).
    pub(super) fn samples(
        &self,
        run: Option<usize>,
    ) -> Result<(Vec<Sample>, Vec<Option<usize>>), DevError> {
        match self {
            Self::Typed { store, id } => {
                let samples = store.query_sidecar_samples(id, run)?;
                let runs = vec![None; samples.len()];
                Ok((samples, runs))
            }
            Self::Bench(bench) => {
                let (samples, runs) = bench.samples(run);
                Ok((samples, runs.into_iter().map(Some).collect()))
            }
        }
    }

    pub(super) fn markers(&self, run: Option<usize>) -> Result<Vec<Marker>, DevError> {
        match self {
            Self::Typed { store, id } => store.query_sidecar_markers(id, run),
            Self::Bench(bench) => Ok(bench.markers(run)),
        }
    }

    pub(super) fn counters(&self, run: Option<usize>) -> Result<Vec<Counter>, DevError> {
        match self {
            Self::Typed { store, id } => store.query_sidecar_counters(id, run),
            Self::Bench(bench) => Ok(bench.counters(run)),
        }
    }

    /// A library call's stored boundaries; a bench child has none.
    pub(super) fn calls(&self, run: Option<usize>) -> Result<Vec<CallSpan>, DevError> {
        match self {
            Self::Typed { store, id } => store.query_sidecar_calls(id, run),
            Self::Bench(_) => Ok(Vec::new()),
        }
    }

    pub(super) fn sink_ends(&self, run: Option<usize>) -> Result<Vec<StoredSinkEnd>, DevError> {
        match self {
            Self::Typed { store, id } => store.query_sidecar_sink_ends(id, run),
            Self::Bench(bench) => Ok(bench.sink_ends(run)),
        }
    }

    /// What is known of run `run`'s process.
    pub(super) fn process(&self, run: usize) -> Process {
        match self {
            Self::Typed { .. } => Process::Sampled,
            Self::Bench(bench) => bench.process(run),
        }
    }

    /// The wall `--stalls` reads against: a typed run's sample span, a bench
    /// child's actual wall.
    pub(super) fn stall_wall_us(&self, run: usize, samples: &[Sample]) -> i64 {
        match self.process(run).wall_us() {
            Some(wall_us) => wall_us,
            None => samples
                .first()
                .zip(samples.last())
                .map_or(0, |(a, b)| b.timestamp_us - a.timestamp_us),
        }
    }

    /// Refuse a `--call` that names no stream in the selected runs, looking
    /// at every kind of record, before any view filter narrows them.
    pub(super) fn check_call(
        &self,
        run: Option<usize>,
        call: Option<&str>,
    ) -> Result<(), DevError> {
        let Some(call) = call else {
            return Ok(());
        };
        if !crate::sidecar::is_valid_call_id(call) {
            return Err(DevError::Usage(format!(
                "--call: '{call}' is not a call id (1 to {} bytes of A-Z a-z 0-9 _ . -)",
                crate::sidecar::MAX_CALL_ID_BYTES
            )));
        }
        if let Self::Bench(bench) = self {
            return bench.check_call(run, call);
        }
        // A call is known by its boundaries, its markers, its counters or its
        // sink records: a sealed call with an empty snapshot may emit nothing
        // but its terminal record.
        let known = self.calls(run)?.iter().any(|s| s.call == call)
            || self
                .markers(run)?
                .iter()
                .any(|m| m.call.as_deref() == Some(call))
            || self
                .counters(run)?
                .iter()
                .any(|c| c.call.as_deref() == Some(call))
            || self.sink_ends(run)?.iter().any(|s| s.call == call);
        if known {
            Ok(())
        } else {
            Err(DevError::Config(format!(
                "--call: no call '{call}' in the selected runs"
            )))
        }
    }
}
