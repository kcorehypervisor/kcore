//! Pure cancel / dual-guest safety policy for persisted VM operations.
//!
//! Cloud Hypervisor send is blocking and may leave the guest only on the
//! destination; cancel after that point is refused.

use crate::db::VmOperationRow;

/// Whether `CancelVmOperation` may succeed for this open op.
pub fn cancel_allowed(phase: &str, send_succeeded: bool) -> bool {
    if send_succeeded {
        return false;
    }
    phase == VmOperationRow::PHASE_PREPARING
}

/// True when the guest would be treated as still runnable on source after send
/// succeeded — the forbidden dual-running state.
pub fn dual_running_guest_forbidden(send_succeeded: bool, source_still_running: bool) -> bool {
    send_succeeded && source_still_running
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    #[test]
    fn preparing_without_send_is_cancellable() {
        assert!(cancel_allowed(VmOperationRow::PHASE_PREPARING, false));
    }

    #[test]
    fn send_succeeded_never_cancellable() {
        for phase in [
            VmOperationRow::PHASE_PREPARING,
            VmOperationRow::PHASE_SENDING,
            VmOperationRow::PHASE_WAITING,
            VmOperationRow::PHASE_REASSIGNED,
            VmOperationRow::PHASE_FINALIZING_DEST,
            VmOperationRow::PHASE_FINALIZING_SOURCE,
            VmOperationRow::PHASE_DONE,
            VmOperationRow::PHASE_FAILED,
        ] {
            assert!(
                !cancel_allowed(phase, true),
                "phase {phase} with send_succeeded must refuse cancel"
            );
        }
    }

    #[test]
    fn sending_without_flag_still_refused() {
        // Even before the DB flag flips, Sending is not a cancel window.
        assert!(!cancel_allowed(VmOperationRow::PHASE_SENDING, false));
    }

    proptest! {
        #![proptest_config(ProptestConfig::with_cases(256))]

        #[test]
        fn cancel_never_when_send_succeeded(
            phase in prop::sample::select(vec![
                VmOperationRow::PHASE_PREPARING,
                VmOperationRow::PHASE_SENDING,
                VmOperationRow::PHASE_WAITING,
                VmOperationRow::PHASE_REASSIGNED,
                VmOperationRow::PHASE_FINALIZING_DEST,
                VmOperationRow::PHASE_FINALIZING_SOURCE,
                VmOperationRow::PHASE_DONE,
                VmOperationRow::PHASE_FAILED,
                VmOperationRow::PHASE_CANCELLED,
                "Unknown",
            ]),
        ) {
            prop_assert!(!cancel_allowed(phase, true));
        }

        #[test]
        fn dual_running_forbidden_after_send(
            source_running in any::<bool>(),
        ) {
            // After send, a still-running source is exactly the forbidden state.
            prop_assert_eq!(
                dual_running_guest_forbidden(true, source_running),
                source_running
            );
            prop_assert!(!dual_running_guest_forbidden(false, source_running));
        }
    }
}
