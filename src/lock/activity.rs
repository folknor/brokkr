//! Holder-published activities: the named steps `brokkr lock` reports as what
//! the holder is doing right now.
//!
//! An [`Activity`] is a scope guard. Opening one records a label (and,
//! optionally, done/total progress) on the current hold's [`Observed`] table;
//! dropping it removes the entry. With no hold, or after the hold is gone, an
//! activity is inert - it holds a `Weak` and never keeps the lock alive.
//!
//! Nesting follows the opening thread: an activity's parent is the innermost
//! activity this thread still has open on the same hold. Threads do not
//! inherit that, so work fanned out to worker threads names its parent
//! explicitly through [`Activity::begin_under`] with an [`ActivityHandle`].
//!
//! [`narrate`] attaches status text to the innermost activity of the calling
//! thread, or to the hold's root narration when the thread has none open.
//! `output::status` calls it on every status line, so the transient terminal
//! line and `brokkr lock` say the same thing - but the lifecycle belongs to
//! the activity scopes, never to the narration.

use std::cell::RefCell;
use std::sync::{Arc, Weak};

use super::observe::Observed;

thread_local! {
    /// This thread's open activities, outermost first.
    static STACK: RefCell<Vec<(Weak<Observed>, u64)>> = const { RefCell::new(Vec::new()) };
}

/// A sendable reference to an open activity, for naming it as the parent of
/// work on another thread.
#[derive(Clone)]
pub(crate) struct ActivityHandle {
    observed: Weak<Observed>,
    id: Option<u64>,
}

/// An open activity. Closed on drop.
#[must_use = "an activity is closed when its guard drops"]
pub(crate) struct Activity {
    observed: Weak<Observed>,
    id: Option<u64>,
}

impl Activity {
    /// Open `label` under this thread's innermost activity on the live hold.
    pub(crate) fn begin(label: impl AsRef<str>) -> Self {
        let Some(observed) = super::current_observed() else {
            return Self::inert();
        };
        let parent = innermost_on(&observed);
        Self::open(&observed, parent, label.as_ref())
    }

    /// Open `label` under `parent`, which may belong to another thread.
    pub(crate) fn begin_under(parent: &ActivityHandle, label: impl AsRef<str>) -> Self {
        let Some(observed) = parent.observed.upgrade() else {
            return Self::begin(label);
        };
        Self::open(&observed, parent.id, label.as_ref())
    }

    fn inert() -> Self {
        Self {
            observed: Weak::new(),
            id: None,
        }
    }

    fn open(observed: &Arc<Observed>, parent: Option<u64>, label: &str) -> Self {
        let Some(id) = observed.open(parent, label) else {
            return Self::inert();
        };
        let weak = Arc::downgrade(observed);
        STACK.with(|stack| stack.borrow_mut().push((Weak::clone(&weak), id)));
        Self {
            observed: weak,
            id: Some(id),
        }
    }

    pub(crate) fn handle(&self) -> ActivityHandle {
        ActivityHandle {
            observed: Weak::clone(&self.observed),
            id: self.id,
        }
    }

    /// Record done/total progress (`done` of `total` units).
    pub(crate) fn progress(&self, done: u64, total: u64) {
        if let (Some(observed), Some(id)) = (self.observed.upgrade(), self.id) {
            observed.set_progress(id, done, total);
        }
    }
}

impl Drop for Activity {
    fn drop(&mut self) {
        let Some(id) = self.id else { return };
        // `try_with`: an activity held in another thread-local can be dropped
        // after this thread's stack was torn down at thread exit.
        let _torn_down = STACK.try_with(|stack| {
            let mut stack = stack.borrow_mut();
            if let Some(at) = stack
                .iter()
                .rposition(|(w, i)| *i == id && w.ptr_eq(&self.observed))
            {
                stack.remove(at);
            }
        });
        if let Some(observed) = self.observed.upgrade() {
            observed.close(id);
        }
    }
}

/// This thread's innermost activity on `observed`, if any.
fn innermost_on(observed: &Arc<Observed>) -> Option<u64> {
    let weak = Arc::downgrade(observed);
    STACK.with(|stack| {
        stack
            .borrow()
            .iter()
            .rev()
            .find(|(w, _)| w.ptr_eq(&weak))
            .map(|(_, id)| *id)
    })
}

/// Attach `text` to this thread's innermost activity, or to the hold's root
/// narration when it has none. A no-op without a hold.
pub(crate) fn narrate(text: &str) {
    let Some(observed) = super::current_observed() else {
        return;
    };
    match innermost_on(&observed) {
        Some(id) => observed.narrate_activity(id, text),
        None => observed.narrate_root(text),
    }
}

/// The `BROKKR_EXEC` value for a child about to be spawned with `label`, and
/// the capability of the same hold - one snapshot, so a child never carries
/// one acquisition's authorization and another's cohort. `None` with no hold.
pub(crate) fn launch_context(label: &str) -> Option<LaunchContext> {
    let (observed, capability) = super::current_observed_and_capability()?;
    let under = innermost_on(&observed).and_then(|id| observed.label_path(id));
    Some(LaunchContext {
        exec: observed.launch(label, under),
        capability,
    })
}

pub(crate) struct LaunchContext {
    pub(crate) exec: String,
    pub(crate) capability: Option<super::Nonce>,
}

/// Record the sidecar's latest phase marker (`None` clears it at a run
/// boundary) on the live hold.
pub(crate) fn note_marker(name: Option<&str>) {
    if let Some(observed) = super::current_observed() {
        observed.note_marker(name);
    }
}
