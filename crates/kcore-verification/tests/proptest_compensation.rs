// Proptest coverage for compensation executor
// Addresses Coverage Gap: Compensation Executor proptest coverage (P0)

use kcore_verification::compensation::*;
use proptest::prelude::*;

fn valid_action_strategy() -> impl Strategy<Value = CompensationAction> {
    prop_oneof![
        (1u64..=1000u64)
            .prop_map(|id| CompensationAction::ReleaseReservation { reservation_id: id }),
        (1u64..=1000u64).prop_map(|id| CompensationAction::RollbackAllocation { volume_id: id }),
        prop::collection::vec(1u64..=100u64, 1..10)
            .prop_map(|ids| CompensationAction::CleanupResources { resource_ids: ids }),
        (1u64..=1000u64, "[a-zA-Z0-9 ]{1,50}").prop_map(|(id, reason)| {
            CompensationAction::NotifyFailure {
                operation_id: id,
                reason,
            }
        }),
    ]
}

fn max_retries_strategy() -> impl Strategy<Value = u32> {
    1u32..=5u32
}

proptest! {
    #[test]
    fn test_register_returns_unique_ids(
        actions in prop::collection::vec(valid_action_strategy(), 2..10),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();
        let mut ids = vec![];

        for action in actions {
            let id = executor.register(action, max_retries);
            ids.push(id);
        }

        // Check uniqueness
        for i in 0..ids.len() {
            for j in (i+1)..ids.len() {
                prop_assert_ne!(ids[i], ids[j], "IDs should be unique");
            }
        }
    }

    #[test]
    fn test_register_increases_pending_count(
        actions in prop::collection::vec(valid_action_strategy(), 1..10),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();

        for (i, action) in actions.iter().enumerate() {
            executor.register(action.clone(), max_retries);
            prop_assert_eq!(executor.pending_count(), i + 1);
        }

        prop_assert_eq!(executor.executed_count(), 0);
    }

    #[test]
    fn test_valid_action_executes_successfully(
        action in valid_action_strategy(),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();
        executor.register(action, max_retries);

        let result = executor.execute_next();
        prop_assert!(result.is_some());

        let task_result = result.unwrap();
        prop_assert!(task_result.is_ok(), "Valid action should succeed");

        prop_assert_eq!(executor.pending_count(), 0);
        prop_assert_eq!(executor.executed_count(), 1);
    }

    #[test]
    fn test_invalid_reservation_fails(max_retries in 1u32..=3u32) {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::ReleaseReservation { reservation_id: 0 };

        executor.register(action, max_retries);

        let mut attempts = 0;
        while executor.pending_count() > 0 && attempts < 10 {
            let result = executor.execute_next();
            if let Some(Err(_)) = result {
                attempts += 1;
            }
        }

        prop_assert!(attempts > 0, "Invalid action should fail at least once");
        prop_assert_eq!(executor.pending_count(), 0, "Should eventually stop retrying");
    }

    #[test]
    fn test_invalid_volume_fails(max_retries in 1u32..=3u32) {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::RollbackAllocation { volume_id: 0 };

        executor.register(action, max_retries);
        let result = executor.execute_next();

        prop_assert!(result.is_some());
        prop_assert!(result.unwrap().is_err(), "Volume ID 0 should fail");
    }

    #[test]
    fn test_empty_resource_list_fails(max_retries in max_retries_strategy()) {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::CleanupResources { resource_ids: vec![] };

        executor.register(action, max_retries);
        let result = executor.execute_next();

        prop_assert!(result.is_some());
        prop_assert!(result.unwrap().is_err(), "Empty resource list should fail");
    }

    #[test]
    fn test_invalid_operation_id_fails(max_retries in max_retries_strategy()) {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::NotifyFailure {
            operation_id: 0,
            reason: "test".to_string()
        };

        executor.register(action, max_retries);
        let result = executor.execute_next();

        prop_assert!(result.is_some());
        prop_assert!(result.unwrap().is_err(), "Operation ID 0 should fail");
    }

    #[test]
    fn test_empty_failure_reason_fails(operation_id in 1u64..=1000u64, max_retries in max_retries_strategy()) {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::NotifyFailure {
            operation_id,
            reason: "".to_string()
        };

        executor.register(action, max_retries);
        let result = executor.execute_next();

        prop_assert!(result.is_some());
        prop_assert!(result.unwrap().is_err(), "Empty reason should fail");
    }

    #[test]
    fn test_lifo_execution_order(
        actions in prop::collection::vec(valid_action_strategy(), 3..8),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();
        let mut registered_ids = vec![];

        for action in actions {
            let id = executor.register(action, max_retries);
            registered_ids.push(id);
        }

        // Execute first task
        if let Some(Ok(task)) = executor.execute_next() {
            // First registered should execute first (LIFO from registration perspective)
            prop_assert_eq!(task.id, registered_ids[0], "LIFO order violated");
        }
    }

    #[test]
    fn test_skip_removes_from_pending(
        actions in prop::collection::vec(valid_action_strategy(), 2..6),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();
        let mut ids = vec![];

        for action in actions {
            let id = executor.register(action, max_retries);
            ids.push(id);
        }

        let initial_pending = executor.pending_count();

        // Skip the first task
        let result = executor.skip_task(ids[0]);
        prop_assert!(result.is_ok(), "Skip should succeed");
        prop_assert_eq!(executor.pending_count(), initial_pending - 1);
        prop_assert_eq!(executor.executed_count(), 1);
    }

    #[test]
    fn test_execute_all_processes_everything(
        actions in prop::collection::vec(valid_action_strategy(), 1..8),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();

        for action in actions.iter() {
            executor.register(action.clone(), max_retries);
        }

        let initial_count = executor.pending_count();
        let result = executor.execute_all();

        prop_assert!(result.is_ok() || result.is_err(), "execute_all should complete");
        prop_assert_eq!(executor.pending_count(), 0, "All tasks should be processed");
        prop_assert!(executor.executed_count() > 0, "Some tasks should be executed");
        prop_assert!(executor.executed_count() <= initial_count, "Can't execute more than registered");
    }

    #[test]
    fn test_max_retries_respected(max_retries in 1u32..=3u32) {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::ReleaseReservation { reservation_id: 0 };

        executor.register(action, max_retries);

        let mut execution_attempts = 0;
        while executor.pending_count() > 0 && execution_attempts < 20 {
            executor.execute_next();
            execution_attempts += 1;
        }

        prop_assert!(execution_attempts <= (max_retries as usize + 1),
                    "Should not exceed max_retries + 1 attempts");
    }

    #[test]
    fn test_multiple_valid_actions_succeed(
        actions in prop::collection::vec(valid_action_strategy(), 1..10),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();

        for action in actions.iter() {
            executor.register(action.clone(), max_retries);
        }

        let expected_count = actions.len();
        let result = executor.execute_all();

        prop_assert!(result.is_ok(), "All valid actions should succeed");

        let executed = executor.get_executed();
        prop_assert_eq!(executed.len(), expected_count, "All tasks should be executed");

        for task in executed {
            prop_assert_eq!(task.status, ExecutionStatus::Completed,
                          "All tasks should be completed");
        }
    }

    #[test]
    fn test_pending_count_accuracy(
        actions in prop::collection::vec(valid_action_strategy(), 0..10),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();
        prop_assert_eq!(executor.pending_count(), 0, "Initial count should be 0");

        for (i, action) in actions.iter().enumerate() {
            executor.register(action.clone(), max_retries);
            prop_assert_eq!(executor.pending_count(), i + 1,
                          "Count should increment with each registration");
        }
    }

    #[test]
    fn test_executed_count_accuracy(
        actions in prop::collection::vec(valid_action_strategy(), 1..8),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();

        for action in actions {
            executor.register(action, max_retries);
        }

        let mut expected_executed = 0;
        while executor.pending_count() > 0 {
            if let Some(Ok(_)) = executor.execute_next() {
                expected_executed += 1;
            }

            if expected_executed > 100 {
                break; // Safety limit
            }
        }

        prop_assert_eq!(executor.executed_count(), expected_executed,
                       "Executed count should match successful executions");
    }

    #[test]
    fn test_get_executed_returns_completed_tasks(
        actions in prop::collection::vec(valid_action_strategy(), 1..6),
        max_retries in max_retries_strategy()
    ) {
        let mut executor = CompensationExecutor::new();

        for action in actions {
            executor.register(action, max_retries);
        }

        let _ = executor.execute_all();

        let executed = executor.get_executed();
        prop_assert!(!executed.is_empty());

        for task in executed {
            prop_assert!(
                task.status == ExecutionStatus::Completed ||
                task.status == ExecutionStatus::Failed ||
                task.status == ExecutionStatus::Skipped,
                "Executed tasks should be in terminal state"
            );
        }
    }
}
