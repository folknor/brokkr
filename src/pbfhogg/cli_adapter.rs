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
        match self {
            // Simple commands: mode + pbf, no extras
            Self::Inspect {
                mode,
                pbf,
                nodes,
                tags,
                type_filter,
                extended,
                jobs,
                snapshot,
            } => {
                let params = CommandParams {
                    jobs: *jobs,
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::Inspect {
                        nodes: *nodes,
                        tags: *tags,
                        type_filter: type_filter.clone(),
                        extended: *extended,
                    },
                    None,
                    params,
                ))
            }
            Self::CheckRefs { mode, pbf, snapshot } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::CheckRefs, None, params))
            }
            Self::CheckIds { mode, pbf, full, snapshot } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::CheckIds { full: *full }, None, params))
            }
            Self::Sort { mode, pbf, snapshot } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::Sort, None, params))
            }
            Self::Cat {
                mode,
                pbf,
                type_filter,
                dedupe,
                clean,
                snapshot,
            } => {
                // Parse errors become `None` - clap's value_parser catches
                // most bad input upstream; dispatch will surface anything
                // that slips through.
                let tf = type_filter
                    .as_deref()
                    .and_then(|s| crate::pbfhogg::commands::CatTypeFilter::parse(s).ok());
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::Cat {
                        type_filter: tf,
                        dedupe: *dedupe,
                        clean: *clean,
                    },
                    None,
                    params,
                ))
            }
            Self::TagsFilter {
                mode,
                pbf,
                filter,
                omit_referenced,
                invert_match,
                remove_tags,
                input_kind,
                osc_seq,
                snapshot,
                jobs,
            } => {
                let input_kind_osc = input_kind.as_deref() == Some("osc");
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    jobs: *jobs,
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::TagsFilter {
                        filter: filter.clone(),
                        omit_referenced: *omit_referenced,
                        invert_match: *invert_match,
                        remove_tags: *remove_tags,
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
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::Getid {
                        add_referenced: *add_referenced,
                        invert: *invert,
                    },
                    None,
                    params,
                ))
            }
            Self::Getparents { mode, pbf, snapshot } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::Getparents, None, params))
            }
            Self::Renumber { mode, pbf, snapshot } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::Renumber, None, params))
            }
            // MultiExtract carries a `--strategy` axis that may expand to
            // three runs ("all"). The fan-out happens in `main.rs`, same as
            // `extract --strategy all`, so this adapter no longer handles it.
            Self::MultiExtract { .. } => None,
            Self::TimeFilter { mode, pbf, snapshot } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::TimeFilter, None, params))
            }
            Self::BuildGeocodeIndex { mode, pbf, snapshot } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::BuildGeocodeIndex, None, params))
            }

            // Commands with OSC sequence
            Self::MergeChanges {
                mode,
                pbf,
                osc_seq,
                osc_range,
                snapshot,
                simplify,
            } => {
                let params = CommandParams {
                    osc_range: osc_range.clone(),
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::MergeChanges { simplify: *simplify },
                    osc_seq.as_deref(),
                    params,
                ))
            }
            Self::ApplyChanges {
                mode,
                pbf,
                osc_seq,
                snapshot,
                locations_on_ways,
                jobs,
            } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    locations_on_ways: *locations_on_ways,
                    jobs: *jobs,
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
                jobs,
            } => {
                let params = CommandParams {
                    keep_cache: *keep_cache,
                    snapshot: snapshot.clone(),
                    jobs: *jobs,
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
                snapshot,
            } => {
                let params = CommandParams {
                    index_type: index_type.clone(),
                    snapshot: snapshot.clone(),
                    ..Default::default()
                };
                Some((mode, pbf, PbfhoggCommand::AddLocationsToWays, None, params))
            }

            Self::Repack {
                mode,
                pbf,
                snapshot,
                elements_per_blob,
                as_snapshot,
                replace_snapshot,
                force_repack,
            } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    as_snapshot: as_snapshot.clone(),
                    replace_snapshot: *replace_snapshot,
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::Repack {
                        elements_per_blob: *elements_per_blob,
                        force: *force_repack,
                    },
                    None,
                    params,
                ))
            }

            Self::Degrade {
                mode,
                pbf,
                snapshot,
                unsort,
                strip_locations,
                strip_indexdata,
                as_snapshot,
                replace_snapshot,
            } => {
                let params = CommandParams {
                    snapshot: snapshot.clone(),
                    as_snapshot: as_snapshot.clone(),
                    replace_snapshot: *replace_snapshot,
                    ..Default::default()
                };
                Some((
                    mode,
                    pbf,
                    PbfhoggCommand::Degrade {
                        unsort: *unsort,
                        strip_locations: *strip_locations,
                        strip_indexdata: *strip_indexdata,
                    },
                    None,
                    params,
                ))
            }

            _ => None,
        }
    }
}
