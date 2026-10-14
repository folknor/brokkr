//! Route the parsed command into a concrete execution input.
use crate::cli::{Command, ModeArgs, PbfArgs};
use crate::pbfhogg::commands::{
    CatTypeFilter, InspectMode, InspectTypeFilter, PbfhoggCommand, TagsFilterInput,
};
use crate::pbfhogg::context::CommandParams;

// A routed command is built once per process and consumed at once, so the
// size gap between the variants costs nothing worth a box on every arm.
#[allow(clippy::large_enum_variant)]
pub(crate) enum RoutedCommand {
    Pbf {
        mode: ModeArgs,
        pbf: PbfArgs,
        command: PbfhoggCommand,
        osc: Option<String>,
        params: CommandParams,
    },
    Other(Box<Command>),
}
impl Command {
    // One arm per pbfhogg-routed variant: the match size tracks the CLI's.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn into_route(self) -> RoutedCommand {
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
                    jobs,
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Inspect {
                        mode: if tags {
                            InspectMode::Tags {
                                type_filter: type_filter
                                    .as_deref()
                                    .map(InspectTypeFilter::from_cli),
                            }
                        } else if nodes {
                            InspectMode::Nodes
                        } else {
                            InspectMode::Default { extended }
                        },
                    },
                    osc: None,
                    params,
                }
            }
            Self::CheckRefs {
                mode,
                pbf,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::CheckRefs,
                    osc: None,
                    params,
                }
            }
            Self::CheckIds {
                mode,
                pbf,
                full,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::CheckIds { full },
                    osc: None,
                    params,
                }
            }
            Self::Sort {
                mode,
                pbf,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Sort,
                    osc: None,
                    params,
                }
            }
            Self::Cat {
                mode,
                pbf,
                type_filter,
                dedupe,
                clean,
                snapshot,
            } => {
                let tf = type_filter.as_deref().map(CatTypeFilter::from_cli);
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Cat {
                        type_filter: tf,
                        dedupe,
                        clean,
                    },
                    osc: None,
                    params,
                }
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
                let input = match input_kind.as_deref() {
                    Some("osc") => TagsFilterInput::Osc,
                    None | Some("pbf") => TagsFilterInput::Pbf {
                        omit_referenced,
                        invert: invert_match,
                        remove_tags,
                    },
                    Some(other) => TagsFilterInput::Invalid(format!(
                        "unknown tags-filter --input-kind '{other}' (expected: pbf, osc)"
                    )),
                };
                let params = CommandParams {
                    snapshot,
                    jobs,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::TagsFilter { filter, input },
                    osc: osc_seq,
                    params,
                }
            }
            Self::Getid {
                mode,
                pbf,
                add_referenced,
                invert,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Getid {
                        add_referenced,
                        invert,
                    },
                    osc: None,
                    params,
                }
            }
            Self::Getparents {
                mode,
                pbf,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Getparents,
                    osc: None,
                    params,
                }
            }
            Self::Renumber {
                mode,
                pbf,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Renumber,
                    osc: None,
                    params,
                }
            }
            // MultiExtract carries a `--strategy` axis that may expand to
            // three runs ("all"). The fan-out happens in `main.rs`, same as
            // `extract --strategy all`, so this adapter no longer handles it.
            Self::MultiExtract { .. } => RoutedCommand::Other(Box::new(self)),
            Self::TimeFilter {
                mode,
                pbf,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::TimeFilter,
                    osc: None,
                    params,
                }
            }
            Self::BuildGeocodeIndex {
                mode,
                pbf,
                snapshot,
            } => {
                let params = CommandParams {
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::BuildGeocodeIndex,
                    osc: None,
                    params,
                }
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
                    osc_range,
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::MergeChanges { simplify },
                    osc: osc_seq,
                    params,
                }
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
                    snapshot,
                    locations_on_ways,
                    jobs,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::ApplyChanges,
                    osc: osc_seq,
                    params,
                }
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
                    keep_cache,
                    snapshot,
                    jobs,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Diff { format },
                    osc: osc_seq,
                    params,
                }
            }

            // Command with extra params
            Self::AddLocationsToWays {
                mode,
                pbf,
                index_type,
                inject_prepass,
                force_altw,
                snapshot,
            } => {
                let params = CommandParams {
                    index_type,
                    inject_prepass,
                    force_altw,
                    snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::AddLocationsToWays,
                    osc: None,
                    params,
                }
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
                    snapshot,
                    as_snapshot,
                    replace_snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Repack {
                        elements_per_blob,
                        force: force_repack,
                    },
                    osc: None,
                    params,
                }
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
                    snapshot,
                    as_snapshot,
                    replace_snapshot,
                    ..Default::default()
                };
                RoutedCommand::Pbf {
                    mode,
                    pbf,
                    command: PbfhoggCommand::Degrade {
                        unsort,
                        strip_locations,
                        strip_indexdata,
                    },
                    osc: None,
                    params,
                }
            }

            // Commands outside the pbfhogg adapter keep their parsed CLI value.
            command => RoutedCommand::Other(Box::new(command)),
        }
    }
}
