//! `brokkr results`' mark flags: one [`store::MarkRequest`] minted per
//! command and appended to the bench store, the local file or the results
//! server the user-wide config names, under the publication latch
//! (`super::publication`). The request is minted once, so every retry is the
//! same request under the same uuid, and a replay of one that landed answers
//! `already_applied`. Admission (`store::marks`) decides whether the store
//! takes it; nothing here judges the state.

use super::publication::{self, Subject};
use super::store::{self, Mark, MarkRequest, Marked};
use crate::error::DevError;

/// A mark as `brokkr results` was asked for it. A run is named as the user
/// gave it, a uuid prefix, until [`MarkAction::resolved`] replaces it with
/// the full uuid the store's mark carries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MarkAction {
    Retire { name: String, reason: String },
    Unretire { name: String },
    Invalidate { run: String, reason: String },
    Revalidate { run: String },
}

impl MarkAction {
    fn mark(&self) -> Mark {
        match self {
            Self::Retire { name, reason } => Mark::Retire {
                name: name.clone(),
                reason: reason.clone(),
            },
            Self::Unretire { name } => Mark::Unretire { name: name.clone() },
            Self::Invalidate { run, reason } => Mark::Invalidate {
                run: run.clone(),
                reason: reason.clone(),
            },
            Self::Revalidate { run } => Mark::Revalidate { run: run.clone() },
        }
    }

    /// The run this action names, if it names one.
    fn run_mut(&mut self) -> Option<&mut String> {
        match self {
            Self::Invalidate { run, .. } | Self::Revalidate { run } => Some(run),
            Self::Retire { .. } | Self::Unretire { .. } => None,
        }
    }

    /// This action with the run it names, a uuid prefix, replaced by the full
    /// uuid of the one run of `project` it starts, hidden or not. Every run
    /// counts whatever the action, so a prefix names the same run for
    /// invalidate and revalidate alike, and whether that run may take the
    /// mark is admission's to say. Resolution is not part of the request:
    /// a run uuid never changes, so a run published after this read cannot
    /// retarget it.
    fn resolved(
        self,
        project: &str,
        server: Option<&crate::config::ResultsServer>,
    ) -> Result<Self, DevError> {
        let mut action = self;
        if action.run_mut().is_none() {
            return Ok(action);
        }
        let reader = super::results::BenchReader::open(project, server)?;
        action.resolved_by(project, reader.as_ref())?;
        Ok(action)
    }

    /// [`Self::resolved`] against `reader`, `None` when there is no store.
    fn resolved_by(
        &mut self,
        project: &str,
        reader: Option<&super::results::BenchReader>,
    ) -> Result<(), DevError> {
        let Some(run) = self.run_mut() else {
            return Ok(());
        };
        let Some(reader) = reader else {
            return Err(DevError::Database(
                "no bench store yet: nothing has been published to mark".into(),
            ));
        };
        let mut uuids = reader.named_by(run)?;
        match uuids.len() {
            1 => {
                *run = uuids.remove(0);
                Ok(())
            }
            0 => {
                let elsewhere = reader.elsewhere(run)?;
                let mut message = format!(
                    "no script-bench run of project {project} starts with {run}; invalidate and revalidate apply to script-bench runs only"
                );
                for (uuid, other) in elsewhere {
                    for piece in ["\n  ", &uuid, " is a run of project ", &other] {
                        message.push_str(piece);
                    }
                }
                Err(DevError::Usage(message))
            }
            n => {
                // Full uuids: two runs may share every digit a view shows.
                let mut message =
                    format!("{run} starts {n} runs of project {project}; give more digits:");
                for uuid in uuids {
                    message.push_str("\n  ");
                    message.push_str(&uuid);
                }
                Err(DevError::Usage(message))
            }
        }
    }

    /// What the mark does, as messages name it.
    fn described(&self) -> String {
        match self {
            Self::Retire { name, .. } => format!("retire {name}"),
            Self::Unretire { name } => format!("unretire {name}"),
            Self::Invalidate { run, .. } => format!("invalidate {}", crate::db::short_uuid(run)),
            Self::Revalidate { run } => format!("revalidate {}", crate::db::short_uuid(run)),
        }
    }

    /// The line a stored mark is reported with. It says what the mark does,
    /// not what is visible now: a replay's mark may since have been undone,
    /// and an unretired bench's invalidated runs stay hidden.
    fn done(&self) -> String {
        match self {
            Self::Retire { name, .. } => format!(
                "recorded: retire {name}, which leaves its runs, and any published under it later, out of listings and --compare (--hidden shows them, --unretire {name} undoes it)"
            ),
            Self::Unretire { name } => {
                format!(
                    "recorded: unretire {name}, which no longer hides its runs by retirement (invalidated runs stay hidden)"
                )
            }
            Self::Invalidate { run, .. } => {
                let short = crate::db::short_uuid(run);
                format!(
                    "recorded: invalidate {short}, which leaves the run out of listings and --compare (--hidden shows it, --revalidate {short} undoes it)"
                )
            }
            Self::Revalidate { run } => format!(
                "recorded: revalidate {}, which no longer hides the run by invalidation (a retired bench's runs stay hidden)",
                crate::db::short_uuid(run)
            ),
        }
    }
}

/// Mint the request for `action` in `project`: a fresh uuid, now, this host.
fn mint(action: &MarkAction, project: &str) -> Result<MarkRequest, DevError> {
    let at_unix_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX));
    Ok(MarkRequest {
        uuid: crate::db::generate_uuid()?,
        project: project.to_owned(),
        at_unix_ms,
        hostname: crate::config::hostname()?,
        mark: action.mark(),
    })
}

/// Append `action` to `project`'s marks, and report it. A store that does
/// not exist yet holds nothing to mark, and is not created.
pub(crate) fn apply(
    action: &MarkAction,
    project: &str,
    server: Option<&crate::config::ResultsServer>,
) -> Result<(), DevError> {
    let action = &action.clone().resolved(project, server)?;
    let request = mint(action, project)?;
    let named = format!(
        "mark {} ({})",
        crate::db::short_uuid(&request.uuid),
        action.described()
    );
    // A fresh command is a new request, never a replay of this one, so an
    // unknown outcome is answered by looking, not by repeating.
    let hint = "inspect the current state with `brokkr results --hidden`, which cannot say whether that attempt may still land; running the command again makes a new request, not a retry";
    let subject = Subject {
        named: &named,
        uncertain_hint: hint,
    };
    let marked = match server {
        Some(server) => super::remote::Client::new(server).mark(&request, &subject)?,
        None => Some(local(&super::evidence::store_path()?, &request, &subject)?),
    };
    crate::output::result_msg(&action.done());
    if marked == Some(Marked::AlreadyApplied) {
        crate::output::result_msg(&format!(
            "{named} had already landed on an earlier attempt; a later mark may have changed its effect since"
        ));
    }
    Ok(())
}

/// Append `request` to the local store at `path`.
fn local(
    path: &std::path::Path,
    request: &MarkRequest,
    subject: &Subject<'_>,
) -> Result<Marked, DevError> {
    let place = path.display().to_string();
    let named_err = |e: &store::StoreError| DevError::Database(format!("{place}: {e}"));
    let Some(mut store) = store::Store::open_existing(path).map_err(|e| named_err(&e))? else {
        return Err(DevError::Database(format!(
            "no bench store at {place} yet: nothing has been published to mark"
        )));
    };
    let mut marked = None;
    publication::write(subject, &place, publication::Policy::LOCAL, || {
        let result = store.mark(request);
        if let Ok(m) = &result {
            marked = Some(*m);
        }
        publication::local_attempt(result.as_ref().map(drop))
    })?;
    marked
        .ok_or_else(|| DevError::Database(format!("{}: settled without an answer", subject.named)))
}

#[cfg(test)]
#[path = "marking_tests.rs"]
mod tests;
