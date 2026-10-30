//! Pairing screen state machine. Pure: fed observations of the node/daemon, no I/O.

use clipsync_core::pairing::{PairingStatus, MAX_FAILED_ATTEMPTS};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Phase {
    /// No pairing in progress.
    Idle,
    /// Window open, looking for / waiting for the other device.
    Searching,
    /// A peer connected; the user must compare `sas` and answer.
    Confirm { sas: String },
    /// This user answered; waiting for the peer's answer.
    Waiting { sas: String },
    Paired,
    /// An attempt failed (mismatch, rejection or timeout) but the window is still open.
    Failed,
    /// Too many failed attempts; the session is locked until the user starts again.
    Locked,
    /// The 2-minute window ran out.
    Expired,
}

/// What the backend currently sees.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Observation {
    pub status: PairingStatus,
    /// SAS of a connected, not yet answered attempt.
    pub pending_sas: Option<String>,
    /// Outcome of a finished attempt, reported once.
    pub finished: Option<Result<(), String>>,
}

#[derive(Debug)]
pub struct PairingUi {
    phase: Phase,
    message: Option<String>,
}

impl Default for PairingUi {
    fn default() -> Self {
        PairingUi { phase: Phase::Idle, message: None }
    }
}

impl PairingUi {
    pub fn phase(&self) -> &Phase {
        &self.phase
    }

    /// Human-readable detail for failures.
    pub fn message(&self) -> Option<&str> {
        self.message.as_deref()
    }

    /// True while the backend should keep (or has) a pairing window open.
    pub fn is_active(&self) -> bool {
        matches!(
            self.phase,
            Phase::Searching | Phase::Confirm { .. } | Phase::Waiting { .. } | Phase::Failed
        )
    }

    /// "Pair New Device" clicked.
    pub fn start(&mut self) {
        self.phase = Phase::Searching;
        self.message = None;
    }

    /// Cancel or dismiss.
    pub fn cancel(&mut self) {
        self.phase = Phase::Idle;
        self.message = None;
    }

    /// The user answered the SAS prompt. Returns the SAS if an answer was expected.
    pub fn answer(&mut self, confirmed: bool) -> bool {
        match std::mem::replace(&mut self.phase, Phase::Idle) {
            Phase::Confirm { sas } if confirmed => {
                self.phase = Phase::Waiting { sas };
                true
            }
            Phase::Confirm { .. } => {
                // Rejection: the backend reports the failed attempt shortly.
                self.phase = Phase::Failed;
                self.message = Some("You rejected the code.".into());
                true
            }
            other => {
                self.phase = other;
                false
            }
        }
    }

    pub fn update(&mut self, obs: &Observation) {
        if let Some(result) = &obs.finished {
            match result {
                Ok(()) => {
                    self.phase = Phase::Paired;
                    self.message = None;
                }
                Err(e) if self.is_active() || self.phase == Phase::Idle => {
                    self.phase = Phase::Failed;
                    self.message = Some(e.clone());
                }
                Err(_) => {}
            }
        }
        if !self.is_active() && self.phase != Phase::Failed {
            return;
        }
        if obs.status.locked {
            self.phase = Phase::Locked;
            self.message = Some(format!(
                "Too many failed attempts ({MAX_FAILED_ATTEMPTS} of {MAX_FAILED_ATTEMPTS}). \
                 Pairing is locked; start again to retry."
            ));
            return;
        }
        match (&self.phase, &obs.pending_sas) {
            (Phase::Searching | Phase::Failed, Some(sas)) => {
                self.phase = Phase::Confirm { sas: sas.clone() };
                self.message = None;
            }
            (Phase::Searching | Phase::Failed | Phase::Confirm { .. } | Phase::Waiting { .. }, _)
                if obs.status.open_for.is_none() =>
            {
                self.phase = Phase::Expired;
                self.message = Some("The 2-minute pairing window ended.".into());
            }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn open() -> PairingStatus {
        PairingStatus { open_for: Some(Duration::from_secs(100)), failed: 0, locked: false }
    }

    fn obs(status: PairingStatus, sas: Option<&str>, finished: Option<Result<(), String>>) -> Observation {
        Observation { status, pending_sas: sas.map(String::from), finished }
    }

    #[test]
    fn starts_idle_and_searching_after_start() {
        let mut ui = PairingUi::default();
        assert_eq!(ui.phase(), &Phase::Idle);
        ui.start();
        assert_eq!(ui.phase(), &Phase::Searching);
    }

    #[test]
    fn idle_ignores_observations() {
        let mut ui = PairingUi::default();
        ui.update(&obs(open(), Some("123456"), None));
        assert_eq!(ui.phase(), &Phase::Idle);
    }

    #[test]
    fn happy_path_search_confirm_wait_paired() {
        let mut ui = PairingUi::default();
        ui.start();
        ui.update(&obs(open(), None, None));
        assert_eq!(ui.phase(), &Phase::Searching);
        ui.update(&obs(open(), Some("042517"), None));
        assert_eq!(ui.phase(), &Phase::Confirm { sas: "042517".into() });
        assert!(ui.answer(true));
        assert_eq!(ui.phase(), &Phase::Waiting { sas: "042517".into() });
        // still pending while the peer thinks
        ui.update(&obs(open(), None, None));
        assert_eq!(ui.phase(), &Phase::Waiting { sas: "042517".into() });
        ui.update(&obs(open(), None, Some(Ok(()))));
        assert_eq!(ui.phase(), &Phase::Paired);
    }

    #[test]
    fn answering_without_a_prompt_is_refused() {
        let mut ui = PairingUi::default();
        ui.start();
        assert!(!ui.answer(true));
        assert_eq!(ui.phase(), &Phase::Searching);
    }

    #[test]
    fn rejecting_shows_failure_and_keeps_searching_while_window_is_open() {
        let mut ui = PairingUi::default();
        ui.start();
        ui.update(&obs(open(), Some("111111"), None));
        assert!(ui.answer(false));
        assert_eq!(ui.phase(), &Phase::Failed);
        assert!(ui.is_active());
        // a new attempt can arrive
        ui.update(&obs(open(), Some("222222"), None));
        assert_eq!(ui.phase(), &Phase::Confirm { sas: "222222".into() });
    }

    #[test]
    fn failed_attempt_reported_by_backend_returns_to_failed_with_reason() {
        let mut ui = PairingUi::default();
        ui.start();
        ui.update(&obs(open(), Some("111111"), None));
        ui.answer(true);
        let st = PairingStatus { failed: 1, ..open() };
        ui.update(&obs(st, None, Some(Err("The other device did not confirm.".into()))));
        assert_eq!(ui.phase(), &Phase::Failed);
        assert_eq!(ui.message(), Some("The other device did not confirm."));
    }

    #[test]
    fn third_failure_locks_and_says_so() {
        let mut ui = PairingUi::default();
        ui.start();
        ui.update(&obs(open(), Some("111111"), None));
        ui.answer(true);
        let locked = PairingStatus { open_for: None, failed: 3, locked: true };
        ui.update(&obs(locked, None, Some(Err("mismatch".into()))));
        assert_eq!(ui.phase(), &Phase::Locked);
        assert!(ui.message().unwrap().contains("locked"));
        assert!(!ui.is_active());
        // stays locked until the user starts again
        ui.update(&obs(open(), Some("999999"), None));
        assert_eq!(ui.phase(), &Phase::Locked);
        ui.start();
        assert_eq!(ui.phase(), &Phase::Searching);
    }

    #[test]
    fn window_running_out_expires_searching_and_pending_prompts() {
        let closed = PairingStatus { open_for: None, failed: 0, locked: false };
        let mut ui = PairingUi::default();
        ui.start();
        ui.update(&obs(closed, None, None));
        assert_eq!(ui.phase(), &Phase::Expired);

        ui.start();
        ui.update(&obs(open(), Some("123456"), None));
        ui.update(&obs(closed, Some("123456"), None));
        assert_eq!(ui.phase(), &Phase::Expired);
    }

    #[test]
    fn cancel_returns_to_idle() {
        let mut ui = PairingUi::default();
        ui.start();
        ui.cancel();
        assert_eq!(ui.phase(), &Phase::Idle);
        assert!(!ui.is_active());
    }
}
