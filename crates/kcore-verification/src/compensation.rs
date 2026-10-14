use serde::{Deserialize, Serialize};
use std::collections::VecDeque;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CompensationAction {
    ReleaseReservation { reservation_id: u64 },
    RollbackAllocation { volume_id: u64 },
    CleanupResources { resource_ids: Vec<u64> },
    NotifyFailure { operation_id: u64, reason: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ExecutionStatus {
    Pending,
    Executing,
    Completed,
    Failed,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompensationTask {
    pub id: u64,
    pub action: CompensationAction,
    pub status: ExecutionStatus,
    pub retry_count: u32,
    pub max_retries: u32,
}

pub struct CompensationExecutor {
    tasks: VecDeque<CompensationTask>,
    next_id: u64,
    executed: Vec<CompensationTask>,
}

impl CompensationExecutor {
    pub fn new() -> Self {
        CompensationExecutor {
            tasks: VecDeque::new(),
            next_id: 1,
            executed: Vec::new(),
        }
    }

    pub fn register(&mut self, action: CompensationAction, max_retries: u32) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let task = CompensationTask {
            id,
            action,
            status: ExecutionStatus::Pending,
            retry_count: 0,
            max_retries,
        };

        self.tasks.push_front(task);
        id
    }

    pub fn execute_next(&mut self) -> Option<Result<CompensationTask, String>> {
        let mut task = self.tasks.pop_back()?;

        if task.status == ExecutionStatus::Completed || task.status == ExecutionStatus::Skipped {
            return Some(Ok(task));
        }

        if task.retry_count >= task.max_retries {
            task.status = ExecutionStatus::Failed;
            self.executed.push(task.clone());
            return Some(Err(format!(
                "Task {} exceeded max retries ({})",
                task.id, task.max_retries
            )));
        }

        task.status = ExecutionStatus::Executing;
        task.retry_count += 1;

        let result = self.execute_action(&task.action);

        match result {
            Ok(()) => {
                task.status = ExecutionStatus::Completed;
                self.executed.push(task.clone());
                Some(Ok(task))
            }
            Err(e) => {
                if task.retry_count < task.max_retries {
                    task.status = ExecutionStatus::Pending;
                    self.tasks.push_back(task.clone());
                    Some(Err(format!("Task {} failed, will retry: {}", task.id, e)))
                } else {
                    task.status = ExecutionStatus::Failed;
                    self.executed.push(task.clone());
                    Some(Err(format!("Task {} failed permanently: {}", task.id, e)))
                }
            }
        }
    }

    fn execute_action(&self, action: &CompensationAction) -> Result<(), String> {
        match action {
            CompensationAction::ReleaseReservation { reservation_id } => {
                if *reservation_id == 0 {
                    return Err("Invalid reservation_id: 0".to_string());
                }
                Ok(())
            }
            CompensationAction::RollbackAllocation { volume_id } => {
                if *volume_id == 0 {
                    return Err("Invalid volume_id: 0".to_string());
                }
                Ok(())
            }
            CompensationAction::CleanupResources { resource_ids } => {
                if resource_ids.is_empty() {
                    return Err("No resources to clean up".to_string());
                }
                Ok(())
            }
            CompensationAction::NotifyFailure {
                operation_id,
                reason,
            } => {
                if *operation_id == 0 {
                    return Err("Invalid operation_id: 0".to_string());
                }
                if reason.is_empty() {
                    return Err("Empty failure reason".to_string());
                }
                Ok(())
            }
        }
    }

    pub fn execute_all(&mut self) -> Result<Vec<CompensationTask>, String> {
        let mut results = Vec::new();
        let mut errors = Vec::new();

        while !self.tasks.is_empty() {
            match self.execute_next() {
                Some(Ok(task)) => results.push(task),
                Some(Err(e)) => {
                    errors.push(e);
                }
                None => break,
            }
        }

        if errors.is_empty() {
            Ok(results)
        } else {
            Err(errors.join("; "))
        }
    }

    pub fn skip_task(&mut self, task_id: u64) -> Result<(), String> {
        if let Some(pos) = self.tasks.iter().position(|t| t.id == task_id) {
            let mut task = self.tasks.remove(pos).unwrap();
            task.status = ExecutionStatus::Skipped;
            self.executed.push(task);
            Ok(())
        } else {
            Err(format!("Task {} not found", task_id))
        }
    }

    pub fn pending_count(&self) -> usize {
        self.tasks.len()
    }

    pub fn executed_count(&self) -> usize {
        self.executed.len()
    }

    pub fn get_executed(&self) -> &[CompensationTask] {
        &self.executed
    }
}

impl Default for CompensationExecutor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_register_task() {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::ReleaseReservation {
            reservation_id: 123,
        };

        let id = executor.register(action, 3);
        assert_eq!(id, 1);
        assert_eq!(executor.pending_count(), 1);
    }

    #[test]
    fn test_execute_successful_task() {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::ReleaseReservation {
            reservation_id: 123,
        };

        executor.register(action, 3);

        let result = executor.execute_next();
        assert!(result.is_some());
        assert!(result.unwrap().is_ok());
        assert_eq!(executor.executed_count(), 1);
        assert_eq!(executor.pending_count(), 0);
    }

    #[test]
    fn test_execute_failing_task() {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::ReleaseReservation { reservation_id: 0 };

        executor.register(action, 2);

        let result1 = executor.execute_next();
        assert!(result1.is_some());
        assert!(result1.unwrap().is_err());
        assert_eq!(executor.pending_count(), 1);

        let result2 = executor.execute_next();
        assert!(result2.is_some());
        assert!(result2.unwrap().is_err());
        assert_eq!(executor.pending_count(), 0);
        assert_eq!(executor.executed_count(), 1);
    }

    #[test]
    fn test_execute_all() {
        let mut executor = CompensationExecutor::new();

        executor.register(
            CompensationAction::ReleaseReservation { reservation_id: 1 },
            3,
        );
        executor.register(CompensationAction::RollbackAllocation { volume_id: 2 }, 3);
        executor.register(
            CompensationAction::CleanupResources {
                resource_ids: vec![3, 4],
            },
            3,
        );

        let result = executor.execute_all();
        assert!(result.is_ok());
        let tasks = result.unwrap();
        assert_eq!(tasks.len(), 3);
        assert_eq!(executor.pending_count(), 0);
    }

    #[test]
    fn test_skip_task() {
        let mut executor = CompensationExecutor::new();
        let action = CompensationAction::ReleaseReservation {
            reservation_id: 123,
        };

        let id = executor.register(action, 3);
        assert!(executor.skip_task(id).is_ok());
        assert_eq!(executor.pending_count(), 0);
        assert_eq!(executor.executed_count(), 1);

        let executed = executor.get_executed();
        assert_eq!(executed[0].status, ExecutionStatus::Skipped);
    }

    #[test]
    fn test_lifo_execution_order() {
        let mut executor = CompensationExecutor::new();

        let id1 = executor.register(
            CompensationAction::ReleaseReservation { reservation_id: 1 },
            3,
        );
        let id2 = executor.register(
            CompensationAction::ReleaseReservation { reservation_id: 2 },
            3,
        );
        let id3 = executor.register(
            CompensationAction::ReleaseReservation { reservation_id: 3 },
            3,
        );

        let task1 = executor.execute_next().unwrap().unwrap();
        assert_eq!(task1.id, id1);

        let task2 = executor.execute_next().unwrap().unwrap();
        assert_eq!(task2.id, id2);

        let task3 = executor.execute_next().unwrap().unwrap();
        assert_eq!(task3.id, id3);
    }

    #[test]
    fn retry_budget_of_zero_fails_before_the_action_runs() {
        let mut executor = CompensationExecutor::new();
        let id = executor.register(
            CompensationAction::ReleaseReservation { reservation_id: 1 },
            0,
        );

        let err = executor.execute_next().unwrap().unwrap_err();
        assert!(err.contains("exceeded max retries"));
        assert_eq!(executor.pending_count(), 0);
        assert_eq!(executor.executed_count(), 1);
        assert_eq!(executor.get_executed()[0].id, id);
        assert_eq!(executor.get_executed()[0].status, ExecutionStatus::Failed);
    }

    #[test]
    fn execute_all_joins_permanent_failures() {
        let mut executor = CompensationExecutor::new();
        executor.register(
            CompensationAction::ReleaseReservation { reservation_id: 0 },
            1,
        );
        executor.register(CompensationAction::RollbackAllocation { volume_id: 0 }, 1);

        let err = executor.execute_all().unwrap_err();
        assert!(err.contains("failed permanently"));
        assert!(err.contains(';'));
        assert_eq!(executor.pending_count(), 0);
        assert_eq!(executor.executed_count(), 2);
    }

    #[test]
    fn skip_unknown_task_is_an_error() {
        let mut executor = CompensationExecutor::new();
        let err = executor.skip_task(42).unwrap_err();
        assert!(err.contains("not found"));
        assert_eq!(executor.pending_count(), 0);
        assert_eq!(executor.executed_count(), 0);
    }
}
