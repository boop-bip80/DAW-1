//! Inert, bounded bookkeeping for future MIDI software-instrument cleanup receipts.
//!
//! A request to release notes is NOT proof that a plugin processed the releases.
//! This module stores per-instance observations only. It never calls a plugin,
//! opens an OS device, or publishes an audio-callback cleanup acknowledgment.
//! A future native-host adapter must report `AcceptedAllReleases` only after a
//! nonzero processing block actually consumed every required release event.

use soundcraft_model::TrackId;
use std::num::NonZeroU64;

/// Maximum number of distinct instrument instances in one cleanup request.
pub const MAX_CLEANUP_INSTRUMENTS: usize = 128;

/// Track IDs alone cannot identify an instrument replaced during a session edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InstrumentInstance {
    pub track: TrackId,
    pub incarnation: NonZeroU64,
}

/// A disabled monitor epoch (even and nonzero) and the exact instrument it targeted.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CleanupTicket {
    pub epoch: u64,
    pub instance: InstrumentInstance,
}

/// A queue attempt is valid only until the targeted plugin is skipped or retried.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessAttempt {
    ticket: CleanupTicket,
    serial: NonZeroU64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InstanceReadiness {
    Active,
    Inactive,
    Failed,
    Parked,
    Retired,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum QueueObservation {
    /// The host verified that ALL required NoteOff and sustain-off events are queued.
    AllRequiredReleasesQueued,
    Full,
    Unsupported,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProcessObservation {
    /// Host-native processing succeeded AND consumed every tagged release event.
    AcceptedAllReleases,
    NativeError,
    Skipped,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockReason {
    Inactive,
    Failed,
    Parked,
    RetiredUnverified,
    QueueFull,
    Unsupported,
    NativeError,
    SkippedProcess,
    ZeroFrames,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptPhase {
    Requested,
    Queued,
    Blocked(BlockReason),
    ProcessDelivered,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReceiptError {
    InvalidEpoch,
    EmptyTargets,
    TooManyTargets,
    DuplicateTarget,
    CleanupInProgress,
    StaleTicket,
    StaleAttempt,
    AlreadyDelivered,
    RetiredUnverified,
    AttemptExhausted,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Entry {
    instance: InstrumentInstance,
    phase: ReceiptPhase,
    attempt_serial: u64,
}

/// All counters are derived from fixed-size storage; none claim native delivery.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReceiptSummary {
    pub epoch: u64,
    pub targets: usize,
    pub process_delivered: usize,
    pub blocked: usize,
    pub retired_unverified: usize,
    pub all_process_delivered: bool,
}

/// A single active batch of at most 128 instrument-instance receipts.
///
/// No heap allocation, locks, native processing or OS calls. `all_process_delivered`
/// is deliberately *not* an ACK to the input worker and does not prove audio silence.
pub struct CleanupReceiptBook {
    epoch: u64,
    len: usize,
    entries: [Option<Entry>; MAX_CLEANUP_INSTRUMENTS],
}

impl Default for CleanupReceiptBook {
    fn default() -> Self {
        Self { epoch: 0, len: 0, entries: [None; MAX_CLEANUP_INSTRUMENTS] }
    }
}

impl CleanupReceiptBook {
    /// Start a complete snapshot of affected instances. Failure leaves the old batch
    /// untouched, particularly if even one earlier instrument remains unverified.
    pub fn begin(&mut self, disabled_epoch: u64, targets: &[InstrumentInstance]) -> Result<(), ReceiptError> {
        if disabled_epoch == 0 || disabled_epoch & 1 != 0 || disabled_epoch <= self.epoch {
            return Err(ReceiptError::InvalidEpoch);
        }
        if self.epoch != 0 && !self.summary().all_process_delivered {
            return Err(ReceiptError::CleanupInProgress);
        }
        if targets.is_empty() {
            return Err(ReceiptError::EmptyTargets);
        }
        if targets.len() > MAX_CLEANUP_INSTRUMENTS {
            return Err(ReceiptError::TooManyTargets);
        }
        for (i, instance) in targets.iter().enumerate() {
            if targets[..i].contains(instance) {
                return Err(ReceiptError::DuplicateTarget);
            }
        }
        self.entries.fill(None);
        self.epoch = disabled_epoch;
        self.len = targets.len();
        for (slot, instance) in self.entries.iter_mut().zip(targets) {
            *slot = Some(Entry { instance: *instance, phase: ReceiptPhase::Requested, attempt_serial: 0 });
        }
        Ok(())
    }

    pub fn ticket(&self, instance: InstrumentInstance) -> Option<CleanupTicket> {
        self.find(instance).map(|_| CleanupTicket { epoch: self.epoch, instance })
    }

    pub fn phase(&self, ticket: CleanupTicket) -> Result<ReceiptPhase, ReceiptError> {
        Ok(self.entry(ticket)?.phase)
    }

    /// Called by a *future* host adapter after checking plugin readiness and queue
    /// capacity. Skipped or incomplete queue attempts never produce a process token.
    pub fn observe_queue(
        &mut self,
        ticket: CleanupTicket,
        readiness: InstanceReadiness,
        queued: QueueObservation,
    ) -> Result<Option<ProcessAttempt>, ReceiptError> {
        let entry = self.entry_mut(ticket)?;
        match entry.phase {
            ReceiptPhase::ProcessDelivered => return Err(ReceiptError::AlreadyDelivered),
            ReceiptPhase::Blocked(BlockReason::RetiredUnverified) => return Err(ReceiptError::RetiredUnverified),
            _ => {}
        }
        let reason = match readiness {
            InstanceReadiness::Active => match queued {
                QueueObservation::AllRequiredReleasesQueued => None,
                QueueObservation::Full => Some(BlockReason::QueueFull),
                QueueObservation::Unsupported => Some(BlockReason::Unsupported),
            },
            InstanceReadiness::Inactive => Some(BlockReason::Inactive),
            InstanceReadiness::Failed => Some(BlockReason::Failed),
            InstanceReadiness::Parked => Some(BlockReason::Parked),
            InstanceReadiness::Retired => Some(BlockReason::RetiredUnverified),
        };
        if let Some(reason) = reason {
            entry.phase = ReceiptPhase::Blocked(reason);
            return Ok(None);
        }
        let Some(serial) = entry.attempt_serial.checked_add(1).and_then(NonZeroU64::new) else {
            entry.phase = ReceiptPhase::Blocked(BlockReason::Unsupported);
            return Err(ReceiptError::AttemptExhausted);
        };
        entry.attempt_serial = serial.get();
        entry.phase = ReceiptPhase::Queued;
        Ok(Some(ProcessAttempt { ticket, serial }))
    }

    /// Report the result of the *same* native process attempt. A zero-frame call,
    /// failed native invocation, or a skipped/parked instance cannot report delivery.
    /// Replacing or requeuing an instrument invalidates older attempt tokens.
    pub fn observe_process(&mut self, attempt: ProcessAttempt, frames: usize, result: ProcessObservation) -> Result<ReceiptPhase, ReceiptError> {
        let entry = self.entry_mut(attempt.ticket)?;
        if entry.phase != ReceiptPhase::Queued || entry.attempt_serial != attempt.serial.get() {
            return Err(ReceiptError::StaleAttempt);
        }
        entry.phase = if frames == 0 {
            ReceiptPhase::Blocked(BlockReason::ZeroFrames)
        } else {
            match result {
                ProcessObservation::AcceptedAllReleases => ReceiptPhase::ProcessDelivered,
                ProcessObservation::NativeError => ReceiptPhase::Blocked(BlockReason::NativeError),
                ProcessObservation::Skipped => ReceiptPhase::Blocked(BlockReason::SkippedProcess),
            }
        };
        Ok(entry.phase)
    }

    /// A skipped or retired instance invalidates any outstanding queue attempt.
    pub fn note_unavailable(&mut self, ticket: CleanupTicket, readiness: InstanceReadiness) -> Result<(), ReceiptError> {
        if readiness == InstanceReadiness::Active {
            return Ok(());
        }
        let _ = self.observe_queue(ticket, readiness, QueueObservation::Unsupported)?;
        Ok(())
    }

    pub fn summary(&self) -> ReceiptSummary {
        let mut delivered = 0;
        let mut blocked = 0;
        let mut retired = 0;
        for entry in self.entries.iter().take(self.len).flatten() {
            match entry.phase {
                ReceiptPhase::ProcessDelivered => delivered += 1,
                ReceiptPhase::Blocked(reason) => {
                    blocked += 1;
                    if reason == BlockReason::RetiredUnverified {
                        retired += 1;
                    }
                }
                _ => {}
            }
        }
        ReceiptSummary {
            epoch: self.epoch,
            targets: self.len,
            process_delivered: delivered,
            blocked,
            retired_unverified: retired,
            all_process_delivered: self.len > 0 && delivered == self.len,
        }
    }

    fn find(&self, instance: InstrumentInstance) -> Option<usize> {
        self.entries.iter().take(self.len).position(|entry| entry.is_some_and(|entry| entry.instance == instance))
    }

    fn entry(&self, ticket: CleanupTicket) -> Result<&Entry, ReceiptError> {
        if ticket.epoch != self.epoch || ticket.epoch == 0 {
            return Err(ReceiptError::StaleTicket);
        }
        self.find(ticket.instance).and_then(|i| self.entries.get(i).and_then(Option::as_ref)).ok_or(ReceiptError::StaleTicket)
    }

    fn entry_mut(&mut self, ticket: CleanupTicket) -> Result<&mut Entry, ReceiptError> {
        if ticket.epoch != self.epoch || ticket.epoch == 0 {
            return Err(ReceiptError::StaleTicket);
        }
        let i = self.find(ticket.instance).ok_or(ReceiptError::StaleTicket)?;
        self.entries.get_mut(i).and_then(Option::as_mut).ok_or(ReceiptError::StaleTicket)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn instance(track: u64, incarnation: u64) -> InstrumentInstance {
        InstrumentInstance { track: TrackId(track), incarnation: NonZeroU64::new(incarnation).unwrap() }
    }

    fn queued(book: &mut CleanupReceiptBook, ticket: CleanupTicket) -> ProcessAttempt {
        book.observe_queue(ticket, InstanceReadiness::Active, QueueObservation::AllRequiredReleasesQueued).unwrap().unwrap()
    }

    #[test]
    fn all_instruments_must_have_actual_process_delivery_receipts() {
        let mut book = CleanupReceiptBook::default();
        let (a, b) = (instance(1, 10), instance(2, 20));
        book.begin(4, &[a, b]).unwrap();
        let x_ticket = book.ticket(a).unwrap();
        let y_ticket = book.ticket(b).unwrap();
        let x = queued(&mut book, x_ticket);
        let y = queued(&mut book, y_ticket);
        assert_eq!(book.summary().process_delivered, 0, "a queue event alone is not process delivery");
        book.observe_process(x, 64, ProcessObservation::AcceptedAllReleases).unwrap();
        assert_eq!(book.summary().process_delivered, 1);
        assert!(!book.summary().all_process_delivered);
        book.observe_process(y, 64, ProcessObservation::AcceptedAllReleases).unwrap();
        assert!(book.summary().all_process_delivered);
        assert_eq!(book.summary().targets, 2);
        assert_eq!(book.observe_process(y, 64, ProcessObservation::AcceptedAllReleases), Err(ReceiptError::StaleAttempt));
    }

    #[test]
    fn parked_failed_and_inactive_instruments_never_get_receipts_without_real_processing() {
        let mut book = CleanupReceiptBook::default();
        let keys = [instance(1, 1), instance(2, 1), instance(3, 1)];
        book.begin(6, &keys).unwrap();
        for (key, readiness) in keys.into_iter().zip([InstanceReadiness::Parked, InstanceReadiness::Failed, InstanceReadiness::Inactive]) {
            let ticket = book.ticket(key).unwrap();
            assert_eq!(book.observe_queue(ticket, readiness, QueueObservation::AllRequiredReleasesQueued), Ok(None));
            assert!(matches!(book.phase(ticket), Ok(ReceiptPhase::Blocked(_))));
        }
        assert_eq!(book.summary().blocked, 3);
        assert!(!book.summary().all_process_delivered);
    }

    #[test]
    fn zero_frame_failed_and_skipped_process_stay_unverified_until_retry() {
        let mut book = CleanupReceiptBook::default();
        let a = instance(1, 1);
        book.begin(8, &[a]).unwrap();
        let ticket = book.ticket(a).unwrap();
        for (frames, observation, reason) in [
            (0, ProcessObservation::AcceptedAllReleases, BlockReason::ZeroFrames),
            (64, ProcessObservation::NativeError, BlockReason::NativeError),
            (64, ProcessObservation::Skipped, BlockReason::SkippedProcess),
        ] {
            let attempt = queued(&mut book, ticket);
            assert_eq!(book.observe_process(attempt, frames, observation), Ok(ReceiptPhase::Blocked(reason)));
            assert!(!book.summary().all_process_delivered);
        }
        let attempt = queued(&mut book, ticket);
        assert_eq!(book.observe_process(attempt, 64, ProcessObservation::AcceptedAllReleases), Ok(ReceiptPhase::ProcessDelivered));
        assert!(book.summary().all_process_delivered);
    }

    #[test]
    fn skipped_after_queue_revokes_old_attempt_even_if_rearmed() {
        let mut book = CleanupReceiptBook::default();
        let a = instance(1, 1);
        book.begin(10, &[a]).unwrap();
        let ticket = book.ticket(a).unwrap();
        let first = queued(&mut book, ticket);
        book.note_unavailable(ticket, InstanceReadiness::Parked).unwrap();
        assert_eq!(book.observe_process(first, 64, ProcessObservation::AcceptedAllReleases), Err(ReceiptError::StaleAttempt));
        let second = queued(&mut book, ticket);
        assert_ne!(first, second);
        assert_eq!(book.observe_process(first, 64, ProcessObservation::AcceptedAllReleases), Err(ReceiptError::StaleAttempt));
        book.observe_process(second, 64, ProcessObservation::AcceptedAllReleases).unwrap();
        assert!(book.summary().all_process_delivered);
    }

    #[test]
    fn retirement_is_terminal_unverified_not_a_delivery_ack() {
        let mut book = CleanupReceiptBook::default();
        let a = instance(1, 1);
        book.begin(12, &[a]).unwrap();
        let ticket = book.ticket(a).unwrap();
        let attempt = queued(&mut book, ticket);
        book.note_unavailable(ticket, InstanceReadiness::Retired).unwrap();
        assert_eq!(book.phase(ticket), Ok(ReceiptPhase::Blocked(BlockReason::RetiredUnverified)));
        assert_eq!(book.summary().retired_unverified, 1);
        assert_eq!(book.observe_process(attempt, 64, ProcessObservation::AcceptedAllReleases), Err(ReceiptError::StaleAttempt));
        assert_eq!(
            book.observe_queue(ticket, InstanceReadiness::Active, QueueObservation::AllRequiredReleasesQueued),
            Err(ReceiptError::RetiredUnverified)
        );
        assert!(!book.summary().all_process_delivered);
    }

    #[test]
    fn queue_overflow_requires_a_fresh_queue_and_process_attempt() {
        let mut book = CleanupReceiptBook::default();
        let a = instance(1, 1);
        book.begin(14, &[a]).unwrap();
        let ticket = book.ticket(a).unwrap();
        assert_eq!(book.observe_queue(ticket, InstanceReadiness::Active, QueueObservation::Full), Ok(None));
        assert_eq!(book.phase(ticket), Ok(ReceiptPhase::Blocked(BlockReason::QueueFull)));
        assert!(!book.summary().all_process_delivered);
        let attempt = queued(&mut book, ticket);
        book.observe_process(attempt, 16, ProcessObservation::AcceptedAllReleases).unwrap();
        assert!(book.summary().all_process_delivered);
    }

    #[test]
    fn newer_epochs_and_replaced_instances_reject_stale_tickets() {
        let mut book = CleanupReceiptBook::default();
        let (old, new) = (instance(1, 1), instance(1, 2));
        book.begin(16, &[old]).unwrap();
        let old_ticket = book.ticket(old).unwrap();
        assert_eq!(book.begin(18, &[new]), Err(ReceiptError::CleanupInProgress));
        assert_eq!(book.summary().epoch, 16, "failed begin must not replace pending cleanup");
        let old_attempt = queued(&mut book, old_ticket);
        book.observe_process(old_attempt, 16, ProcessObservation::AcceptedAllReleases).unwrap();
        book.begin(18, &[new]).unwrap();
        let new_ticket = book.ticket(new).unwrap();
        assert_eq!(book.observe_process(old_attempt, 32, ProcessObservation::AcceptedAllReleases), Err(ReceiptError::StaleTicket));
        assert_eq!(book.phase(old_ticket), Err(ReceiptError::StaleTicket));
        assert_eq!(book.ticket(old), None);
        assert_ne!(old_ticket, new_ticket);
        assert!(!book.summary().all_process_delivered);
    }

    #[test]
    fn invalid_empty_duplicate_and_overcapacity_batches_do_not_mutate_state() {
        let mut book = CleanupReceiptBook::default();
        let one = instance(1, 1);
        assert_eq!(book.begin(1, &[one]), Err(ReceiptError::InvalidEpoch));
        assert_eq!(book.begin(2, &[]), Err(ReceiptError::EmptyTargets));
        assert_eq!(book.begin(2, &[one, one]), Err(ReceiptError::DuplicateTarget));
        assert_eq!(book.begin(2, &[one; MAX_CLEANUP_INSTRUMENTS + 1]), Err(ReceiptError::TooManyTargets));
        assert_eq!(book.summary().targets, 0);
        assert!(!book.summary().all_process_delivered);
        book.begin(2, &[one]).unwrap();
        let original = book.summary();
        assert_eq!(book.begin(2, &[one]), Err(ReceiptError::InvalidEpoch));
        assert_eq!(book.begin(4, &[one]), Err(ReceiptError::CleanupInProgress));
        assert_eq!(book.summary(), original);
    }
}
