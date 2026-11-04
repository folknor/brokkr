//! Retrying a request to the bench store, and the publication latch both
//! backends share (`reference/bench-store.md`, What a client owes the
//! contract).
//!
//! A publication's attempt either settles it (stored, already stored, or a
//! remote 200 whatever its body) or fails, saying whether this attempt is
//! proved unapplied and whether repeating it may succeed. Once an attempt is
//! not proved unapplied the publication is uncertain, and stays so across
//! later refusals and proved-unapplied attempts, since the uncertain one may
//! still commit: only a success settles it, and the error names the
//! uncertain attempt, never the refusal that ended the loop.

use std::time::{Duration, Instant};

use crate::error::DevError;

/// When a retry may start. An attempt itself is never cut short by these:
/// the window and the cap only decide whether another one begins.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Policy {
    /// No ordinary attempt starts once this long has passed since the first.
    pub(crate) window: Duration,
    /// Ordinary attempts at most, the first included.
    pub(crate) attempts: u32,
    /// The pause before the second attempt, doubling up to `max_delay`.
    pub(crate) first_delay: Duration,
    pub(crate) max_delay: Duration,
}

impl Policy {
    /// The local store: a commit error is republished at once, a few times.
    pub(crate) const LOCAL: Self = Self {
        window: Duration::from_secs(3600),
        attempts: 4,
        first_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };

    /// A results server, for a publication: a measured run is worth waiting
    /// for, and every retry is idempotent by its uuid.
    pub(crate) const PUBLISH: Self = Self {
        window: Duration::from_secs(120),
        attempts: 8,
        first_delay: Duration::from_millis(250),
        max_delay: Duration::from_secs(8),
    };

    /// A results server, for a read or the destination probe.
    pub(crate) const READ: Self = Self {
        window: Duration::from_secs(20),
        attempts: 6,
        first_delay: Duration::from_millis(250),
        max_delay: Duration::from_secs(4),
    };

    fn delay(self, made: u32) -> Duration {
        let doublings = made.saturating_sub(1).min(16);
        self.first_delay
            .saturating_mul(1 << doublings)
            .min(self.max_delay)
    }
}

/// Whether another attempt starts, counting every attempt made.
pub(crate) struct Admission {
    policy: Policy,
    started: Instant,
    made: u32,
    /// The one attempt past the window or the cap a latched publication may
    /// still make, once spent.
    exception_spent: bool,
}

impl Admission {
    pub(crate) fn new(policy: Policy) -> Self {
        Self {
            policy,
            started: Instant::now(),
            made: 0,
            exception_spent: false,
        }
    }

    /// Count an attempt about to be made.
    pub(crate) fn attempt(&mut self) {
        self.made += 1;
    }

    pub(crate) fn made(&self) -> u32 {
        self.made
    }

    fn ordinary(&self) -> bool {
        self.made < self.policy.attempts && self.started.elapsed() < self.policy.window
    }

    /// After a retryable failure: whether another attempt starts, after its
    /// pause. Nothing is waited for when none would start, and admission is
    /// checked again after the pause. A pending stop ends the retries,
    /// during the pause too. `latched` lets one attempt past the window or
    /// the cap, once, so an uncertain publication always gets a chance to
    /// settle.
    pub(crate) fn another(&mut self, latched: bool) -> bool {
        if crate::shutdown::should_unwind() {
            return false;
        }
        if !self.ordinary() && !(latched && !self.exception_spent) {
            return false;
        }
        let delay = self.policy.delay(self.made);
        let until = Instant::now() + delay;
        while Instant::now() < until {
            if crate::shutdown::should_unwind() {
                return false;
            }
            std::thread::sleep(
                until
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_millis(50)),
            );
        }
        if crate::shutdown::should_unwind() {
            return false;
        }
        if self.ordinary() {
            return true;
        }
        if latched && !self.exception_spent {
            self.exception_spent = true;
            return true;
        }
        false
    }
}

/// How one publication attempt ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Attempt {
    /// The run is stored, now or before.
    Settled,
    Failed {
        /// This attempt is proved not to have applied.
        unapplied: bool,
        /// Repeating it may succeed.
        retry: bool,
        message: String,
    },
}

/// Publish run `uuid` to the store `place` names, one `attempt` at a time,
/// under `policy`, latching uncertainty.
pub(crate) fn publish(
    uuid: &str,
    place: &str,
    policy: Policy,
    mut attempt: impl FnMut() -> Attempt,
) -> Result<(), DevError> {
    let mut admission = Admission::new(policy);
    let mut uncertain: Option<String> = None;
    let last = loop {
        admission.attempt();
        match attempt() {
            Attempt::Settled => {
                if let Some(why) = &uncertain {
                    crate::output::bench_msg(&format!(
                        "run {uuid} stored at {place} on attempt {}, after an attempt whose outcome was unknown ({why})",
                        admission.made()
                    ));
                }
                return Ok(());
            }
            Attempt::Failed {
                unapplied,
                retry,
                message,
            } => {
                if !unapplied {
                    uncertain = Some(message.clone());
                }
                if !retry || !admission.another(uncertain.is_some()) {
                    break message;
                }
            }
        }
    };
    let attempts = crate::output::count(
        usize::try_from(admission.made()).unwrap_or(usize::MAX),
        "attempt",
    );
    Err(DevError::Database(match uncertain {
        Some(why) => format!(
            "run {uuid} may or may not have been stored at {place} (outcome unknown after {attempts}): {why}; `brokkr results {}` shows whether it landed",
            crate::db::short_uuid(uuid)
        ),
        None => format!("run {uuid} was not stored at {place} ({attempts}): {last}"),
    }))
}

/// A local store error as an attempt, by the kind it travels as on the
/// wire: the same latch serves both backends.
pub(crate) fn local_attempt(result: Result<(), &super::store::StoreError>) -> Attempt {
    match result {
        Ok(()) => Attempt::Settled,
        Err(error) => {
            let kind = super::store::wire::Kind::of(error);
            Attempt::Failed {
                unapplied: kind.unapplied(),
                retry: kind.retry(),
                message: error.to_string(),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    const QUICK: Policy = Policy {
        window: Duration::from_secs(60),
        attempts: 3,
        first_delay: Duration::ZERO,
        max_delay: Duration::ZERO,
    };

    fn failed(unapplied: bool, retry: bool, message: &str) -> Attempt {
        Attempt::Failed {
            unapplied,
            retry,
            message: message.into(),
        }
    }

    /// Run the latch over scripted attempts; the error text, and how many
    /// attempts it made.
    fn script(policy: Policy, attempts: Vec<Attempt>) -> (Result<(), String>, usize) {
        let mut queue = std::collections::VecDeque::from(attempts);
        let mut made = 0;
        let result = publish("u", "the store", policy, || {
            made += 1;
            queue.pop_front().unwrap()
        });
        (result.map_err(|e| e.to_string()), made)
    }

    #[test]
    fn a_refusal_before_any_uncertainty_is_not_stored_and_final() {
        let (result, made) = script(QUICK, vec![failed(true, false, "refused: bad")]);
        let error = result.unwrap_err();
        assert!(
            error.contains("run u was not stored at the store") && error.contains("refused: bad"),
            "{error}"
        );
        assert_eq!(made, 1);
    }

    #[test]
    fn uncertainty_survives_later_refusals_and_names_the_uncertain_attempt() {
        let (result, made) = script(
            QUICK,
            vec![
                failed(false, true, "commit_uncertain: disk"),
                failed(true, true, "busy"),
                failed(true, false, "conflict"),
            ],
        );
        let error = result.unwrap_err();
        assert!(error.contains("may or may not have been stored"), "{error}");
        assert!(
            error.contains("commit_uncertain: disk") && !error.contains("conflict"),
            "{error}"
        );
        assert_eq!(made, 3);
    }

    #[test]
    fn only_a_success_settles_an_uncertain_publication() {
        let (result, made) = script(
            QUICK,
            vec![
                failed(false, true, "connection reset"),
                failed(true, true, "busy"),
                Attempt::Settled,
            ],
        );
        result.unwrap();
        assert_eq!(made, 3);
    }

    #[test]
    fn a_latched_publication_gets_one_attempt_past_the_cap_and_no_more() {
        // The cap is reached uncertain: one more attempt, which settles.
        let (result, made) = script(
            QUICK,
            vec![
                failed(true, true, "refused connection"),
                failed(true, true, "refused connection"),
                failed(false, true, "reset"),
                Attempt::Settled,
            ],
        );
        result.unwrap();
        assert_eq!(made, 4);
        // That attempt ending uncertain too stops the loop, still uncertain.
        let (result, made) = script(
            QUICK,
            vec![
                failed(false, true, "reset"),
                failed(false, true, "reset"),
                failed(false, true, "reset again"),
                failed(false, true, "reset a fourth time"),
            ],
        );
        assert!(result.unwrap_err().contains("may or may not"));
        assert_eq!(made, 4);
        // Without uncertainty the cap is the cap.
        let (result, made) = script(
            QUICK,
            vec![
                failed(true, true, "refused connection"),
                failed(true, true, "refused connection"),
                failed(true, true, "refused connection"),
            ],
        );
        assert!(result.unwrap_err().contains("was not stored"));
        assert_eq!(made, 3);
    }

    #[test]
    fn a_closed_window_starts_no_ordinary_retry_but_a_latched_one_gets_its_exception() {
        let closed = Policy {
            window: Duration::ZERO,
            attempts: 10,
            ..QUICK
        };
        let (result, made) = script(closed, vec![failed(true, true, "refused connection")]);
        assert!(result.unwrap_err().contains("was not stored"));
        assert_eq!(made, 1);
        let (result, made) = script(closed, vec![failed(false, true, "reset"), Attempt::Settled]);
        result.unwrap();
        assert_eq!(made, 2);
        // The exception is spent once.
        let (result, made) = script(
            closed,
            vec![
                failed(false, true, "reset"),
                failed(false, true, "reset again"),
            ],
        );
        assert!(result.unwrap_err().contains("may or may not"));
        assert_eq!(made, 2);
    }

    #[test]
    fn a_local_store_error_travels_as_its_wire_kind() {
        use super::super::store::StoreError;
        assert_eq!(local_attempt(Ok(())), Attempt::Settled);
        assert_eq!(
            local_attempt(Err(&StoreError::CommitUncertain("x".into()))),
            failed(
                false,
                true,
                &StoreError::CommitUncertain("x".into()).to_string()
            )
        );
        assert!(matches!(
            local_attempt(Err(&StoreError::Failed("x".into()))),
            Attempt::Failed {
                unapplied: true,
                retry: true,
                ..
            }
        ));
        assert!(matches!(
            local_attempt(Err(&StoreError::Conflict { uuid: "u".into() })),
            Attempt::Failed {
                unapplied: true,
                retry: false,
                ..
            }
        ));
    }
}
