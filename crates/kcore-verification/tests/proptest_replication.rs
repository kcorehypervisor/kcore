// Proptest coverage for replication outbox state machine
// Objective 4: Extended proptest coverage

use kcore_verification::replication::*;
use proptest::prelude::*;

fn reservation_strategy() -> impl Strategy<Value = ReplicationReservation> {
    (
        1u64..=1000u64,
        1u64..=100u64,
        1u64..=100u64,
        1u64..=1_000_000_000u64,
        1u8..=10u8,
    )
        .prop_map(|(vol, src, tgt, bytes, pri)| ReplicationReservation {
            volume_id: VolumeId(vol),
            source_replica: ReplicaId(src),
            target_replica: ReplicaId(tgt),
            reserved_bytes: bytes,
            priority: pri,
        })
}

fn valid_progress() -> impl Strategy<Value = u8> {
    0u8..=100u8
}

proptest! {
    #[test]
    fn test_outbox_enqueue(reservation in reservation_strategy()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation.clone());

        prop_assert!(id > 0);
        let entry = outbox.get(id).unwrap();
        prop_assert_eq!(&entry.state, &ReplicationState::Pending);
        prop_assert_eq!(&entry.reservation, &reservation);
        prop_assert_eq!(entry.retry_count, 0);
    }

    #[test]
    fn test_outbox_start_transition(reservation in reservation_strategy()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        let result = outbox.start(id);
        prop_assert!(result.is_ok());

        let entry = outbox.get(id).unwrap();
        let is_in_progress = matches!(entry.state, ReplicationState::InProgress { progress: 0 });
        prop_assert!(is_in_progress);
    }

    #[test]
    fn test_progress_update(reservation in reservation_strategy(), progress in valid_progress()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        prop_assert!(outbox.start(id).is_ok());
        let result = outbox.update_progress(id, progress);

        prop_assert!(result.is_ok());
        let entry = outbox.get(id).unwrap();
        let is_correct_progress = matches!(&entry.state, ReplicationState::InProgress { progress: p } if *p == progress);
        prop_assert!(is_correct_progress, "Expected progress {}, got {:?}", progress, entry.state);
    }

    #[test]
    fn test_invalid_progress_rejected(reservation in reservation_strategy(), progress in 101u8..=255u8) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        prop_assert!(outbox.start(id).is_ok());
        let result = outbox.update_progress(id, progress);

        prop_assert!(result.is_err());
    }

    #[test]
    fn test_completion_flow(reservation in reservation_strategy()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        prop_assert!(outbox.start(id).is_ok());
        prop_assert!(outbox.update_progress(id, 100).is_ok());

        let result = outbox.complete(id);
        prop_assert!(result.is_ok());

        let entry = outbox.get(id).unwrap();
        prop_assert_eq!(&entry.state, &ReplicationState::Completed);
    }

    #[test]
    fn test_completion_requires_100(reservation in reservation_strategy(), progress in 0u8..100u8) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        prop_assert!(outbox.start(id).is_ok());
        prop_assert!(outbox.update_progress(id, progress).is_ok());

        let result = outbox.complete(id);
        prop_assert!(result.is_err());
    }

    #[test]
    fn test_failure_increments_retry(reservation in reservation_strategy()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        prop_assert!(outbox.start(id).is_ok());

        let initial_retry = outbox.get(id).unwrap().retry_count;
        prop_assert!(outbox.fail(id, "error".to_string()).is_ok());
        let final_retry = outbox.get(id).unwrap().retry_count;

        prop_assert_eq!(final_retry, initial_retry + 1);
    }

    #[test]
    fn test_cancel_from_pending(reservation in reservation_strategy()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        let result = outbox.cancel(id);
        prop_assert!(result.is_ok());

        let entry = outbox.get(id).unwrap();
        prop_assert_eq!(&entry.state, &ReplicationState::Cancelled);
    }

    #[test]
    fn test_cancel_from_in_progress(reservation in reservation_strategy(), progress in valid_progress()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        prop_assert!(outbox.start(id).is_ok());
        prop_assert!(outbox.update_progress(id, progress).is_ok());

        let result = outbox.cancel(id);
        prop_assert!(result.is_ok());

        let entry = outbox.get(id).unwrap();
        prop_assert_eq!(&entry.state, &ReplicationState::Cancelled);
    }

    #[test]
    fn test_invalid_transitions_from_pending(reservation in reservation_strategy(), progress in valid_progress()) {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(reservation);

        prop_assert!(outbox.update_progress(id, progress).is_err());
        prop_assert!(outbox.complete(id).is_err());
    }

    #[test]
    fn test_multiple_entries(
        reservations in prop::collection::vec(reservation_strategy(), 1..10)
    ) {
        let mut outbox = ReplicationOutbox::new();
        let mut ids = Vec::new();

        for reservation in &reservations {
            let id = outbox.enqueue(reservation.clone());
            ids.push(id);
        }

        prop_assert_eq!(ids.len(), reservations.len());

        for (id, reservation) in ids.iter().zip(reservations.iter()) {
            let entry = outbox.get(*id).unwrap();
            prop_assert_eq!(&entry.reservation, reservation);
        }
    }

    #[test]
    fn test_get_pending_filter(reservations in prop::collection::vec(reservation_strategy(), 2..5)) {
        let mut outbox = ReplicationOutbox::new();

        for reservation in reservations {
            outbox.enqueue(reservation);
        }

        let pending = outbox.get_pending();
        prop_assert!(!pending.is_empty());
        prop_assert!(pending.iter().all(|e| matches!(e.state, ReplicationState::Pending)));
    }

    #[test]
    fn test_get_in_progress_filter(reservations in prop::collection::vec(reservation_strategy(), 2..5)) {
        let mut outbox = ReplicationOutbox::new();
        let mut ids = Vec::new();

        for reservation in reservations {
            let id = outbox.enqueue(reservation);
            ids.push(id);
        }

        for id in &ids[..ids.len()/2] {
            let _ = outbox.start(*id);
        }

        let in_progress = outbox.get_in_progress();
        prop_assert!(!in_progress.is_empty());
        let all_in_progress = in_progress.iter().all(|e| matches!(&e.state, ReplicationState::InProgress { .. }));
        prop_assert!(all_in_progress);
    }
}
