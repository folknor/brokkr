//! Adapter from the `clap`-parsed CLI shape (`Command` enum in `cli.rs`) to
//! the typed `PbfhoggCommand` the dispatch layer runs. Kept alongside the
//! pbfhogg command definitions so the two surfaces stay in sync.

use crate::cli::{Command, ModeArgs, PbfArgs};
use crate::measure::CommandParams;
use crate::pbfhogg::commands::PbfhoggCommand;

impl Command {
    /// Extract the pbfhogg measured-command parts from a CLI command variant.
    ///
    /// Returns `None` for non-pbfhogg commands (elivagar, nidhogg, shared, etc.).
    /// The returned tuple is `(mode, pbf, command, osc_seq, params)`.
    #[allow(clippy::too_many_lines, clippy::type_complexity)]
    pub(crate) fn as_pbfhogg(
        &self,
    ) -> Option<(
        &ModeArgs,
        &PbfArgs,
        PbfhoggCommand,
        Option<&str>,
        CommandParams,
    )> {
        let empty = CommandParams::default();
        match self {
            // Simple commands: mode + pbf, no extras
            Self::Inspect {
                mode,
                pbf,
                nodes,
                tags,
                type_filter,
            } => Some((
                mode,
                pbf,
                PbfhoggCommand::Inspect {
                    nodes: *nodes,
                    tags: *tags,
                    type_filter: type_filter.clone(),
                },
                None,
                empty,
            )),
            Self::CheckRefs { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::CheckRefs, None, empty))
            }
            Self::CheckIds { mode, pbf, full } => {
                Some((mode, pbf, PbfhoggCommand::CheckIds { full: *full }, None, empty))
            }
            Self::Sort { mode, pbf } => Some((mode, pbf, PbfhoggCommand::Sort, None, empty)),
            Self::Cat {
                mode,
                pbf,
                type_filter,
                dedupe,
                clean,
            } => {
                // Parse errors become `None` — clap's value_parser catches
                // most bad input upstream; dispatch will surface anything
                // that slips through.
                let tf = type_filter
                    .as_deref()
                    .and_then(|s| crate::pbfhogg::commands::CatTypeFilter::parse(s).ok());
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::Cat {
                        type_filter: tf,
                        dedupe: *dedupe,
                        clean: *clean,
                    },
                    None,
                    empty,
                ))
            }
            Self::TagsFilter {
                mode,
                pbf,
                filter,
                omit_referenced,
                input_kind,
                osc_seq,
                snapshot,
            } => {
                let input_kind_osc = input_kind.as_deref() == Some("osc");
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::TagsFilter {
                        filter: filter.clone(),
                        omit_referenced: *omit_referenced,
                        input_kind_osc,
                    },
                    osc_seq.as_deref(),
                    params,
                ))
            }
            Self::Getid {
                mode,
                pbf,
                add_referenced,
                invert,
            } => Some((
                mode,
                pbf,
                PbfhoggCommand::Getid {
                    add_referenced: *add_referenced,
                    invert: *invert,
                },
                None,
                empty,
            )),
            Self::Getparents { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::Getparents, None, empty))
            }
            Self::Renumber { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::Renumber, None, empty))
            }
            Self::MultiExtract {
                mode,
                pbf,
                regions,
                bbox,
            } => {
                let params = CommandParams {
                    bbox: bbox.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::MultiExtract { regions: *regions },
                    None,
                    params,
                ))
            }
            Self::TimeFilter { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::TimeFilter, None, empty))
            }
            Self::BuildGeocodeIndex { mode, pbf } => {
                Some((mode, pbf, PbfhoggCommand::BuildGeocodeIndex, None, empty))
            }

            // Commands with OSC sequence
            Self::MergeChanges {
                mode,
                pbf,
                osc_seq,
                osc_range,
                snapshot,
            } => {
                let params = CommandParams {
                    osc_range: osc_range.clone(),
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::MergeChanges,
                    osc_seq.as_deref(),
                    params,
                ))
            }
            Self::ApplyChanges {
                mode,
                pbf,
                osc_seq,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::ApplyChanges,
                    osc_seq.as_deref(),
                    params,
                ))
            }
            Self::Diff {
                mode,
                pbf,
                format,
                osc_seq,
                keep_cache,
                snapshot,
            } => {
                let params = CommandParams {
                    keep_cache: *keep_cache,
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::Diff { format: *format },
                    osc_seq.as_deref(),
                    params,
                ))
            }

            // Command with extra params
            Self::AddLocationsToWays {
                mode,
                pbf,
                index_type,
            } => {
                let params = CommandParams {
                    index_type: index_type.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::AddLocationsToWays, None, params))
            }

            _ => None,
        }
    }
}
