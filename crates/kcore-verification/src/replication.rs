use serde::{Deserialize, Serialize};
use std::collections::HashMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ReplicaId(pub u64);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct VolumeId(pub u64);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ReplicationState {
    Pending,
    InProgress { progress: u8 },
    Completed,
    Failed { reason: String },
    Cancelled,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplicationReservation {
    pub volume_id: VolumeId,
    pub source_replica: ReplicaId,
    pub target_replica: ReplicaId,
    pub reserved_bytes: u64,
    pub priority: u8,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutboxEntry {
    pub id: u64,
    pub volume_id: VolumeId,
    pub state: ReplicationState,
    pub reservation: ReplicationReservation,
    pub retry_count: u32,
}

pub struct ReplicationOutbox {
    entries: HashMap<u64, OutboxEntry>,
    next_id: u64,
}

impl ReplicationOutbox {
    pub fn new() -> Self {
        ReplicationOutbox {
            entries: HashMap::new(),
            next_id: 1,
        }
    }

    pub fn enqueue(&mut self, reservation: ReplicationReservation) -> u64 {
        let id = self.next_id;
        self.next_id += 1;

        let entry = OutboxEntry {
            id,
            volume_id: reservation.volume_id,
            state: ReplicationState::Pending,
            reservation,
            retry_count: 0,
        };

        self.entries.insert(id, entry);
        id
    }

    pub fn start(&mut self, id: u64) -> Result<(), String> {
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| "entry not found".to_string())?;

        match entry.state {
            ReplicationState::Pending | ReplicationState::Failed { .. } => {
                entry.state = ReplicationState::InProgress { progress: 0 };
                Ok(())
            }
            _ => Err(format!("cannot start from state: {:?}", entry.state)),
        }
    }

    pub fn update_progress(&mut self, id: u64, progress: u8) -> Result<(), String> {
        if progress > 100 {
            return Err("progress must be <= 100".to_string());
        }

        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| "entry not found".to_string())?;

        match entry.state {
            ReplicationState::InProgress { .. } => {
                entry.state = ReplicationState::InProgress { progress };
                Ok(())
            }
            _ => Err(format!(
                "cannot update progress from state: {:?}",
                entry.state
            )),
        }
    }

    pub fn complete(&mut self, id: u64) -> Result<(), String> {
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| "entry not found".to_string())?;

        match entry.state {
            ReplicationState::InProgress { progress: 100 } => {
                entry.state = ReplicationState::Completed;
                Ok(())
            }
            ReplicationState::InProgress { progress } => {
                Err(format!("cannot complete at progress: {}", progress))
            }
            _ => Err(format!("cannot complete from state: {:?}", entry.state)),
        }
    }

    pub fn fail(&mut self, id: u64, reason: String) -> Result<(), String> {
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| "entry not found".to_string())?;

        match entry.state {
            ReplicationState::Pending | ReplicationState::InProgress { .. } => {
                entry.state = ReplicationState::Failed { reason };
                entry.retry_count += 1;
                Ok(())
            }
            _ => Err(format!("cannot fail from state: {:?}", entry.state)),
        }
    }

    pub fn cancel(&mut self, id: u64) -> Result<(), String> {
        let entry = self
            .entries
            .get_mut(&id)
            .ok_or_else(|| "entry not found".to_string())?;

        match entry.state {
            ReplicationState::Pending | ReplicationState::InProgress { .. } => {
                entry.state = ReplicationState::Cancelled;
                Ok(())
            }
            _ => Err(format!("cannot cancel from state: {:?}", entry.state)),
        }
    }

    pub fn get(&self, id: u64) -> Option<&OutboxEntry> {
        self.entries.get(&id)
    }

    pub fn get_pending(&self) -> Vec<&OutboxEntry> {
        self.entries
            .values()
            .filter(|e| matches!(e.state, ReplicationState::Pending))
            .collect()
    }

    pub fn get_in_progress(&self) -> Vec<&OutboxEntry> {
        self.entries
            .values()
            .filter(|e| matches!(e.state, ReplicationState::InProgress { .. }))
            .collect()
    }
}

impl Default for ReplicationOutbox {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_test_reservation() -> ReplicationReservation {
        ReplicationReservation {
            volume_id: VolumeId(1),
            source_replica: ReplicaId(10),
            target_replica: ReplicaId(20),
            reserved_bytes: 1024 * 1024 * 1024,
            priority: 5,
        }
    }

    #[test]
    fn test_outbox_enqueue() {
        let mut outbox = ReplicationOutbox::new();
        let reservation = create_test_reservation();

        let id = outbox.enqueue(reservation.clone());
        assert_eq!(id, 1);

        let entry = outbox.get(id).unwrap();
        assert_eq!(entry.state, ReplicationState::Pending);
        assert_eq!(entry.reservation, reservation);
    }

    #[test]
    fn test_outbox_state_transitions() {
        let mut outbox = ReplicationOutbox::new();
        let reservation = create_test_reservation();
        let id = outbox.enqueue(reservation);

        assert!(outbox.start(id).is_ok());
        assert!(matches!(
            outbox.get(id).unwrap().state,
            ReplicationState::InProgress { progress: 0 }
        ));

        assert!(outbox.update_progress(id, 50).is_ok());
        assert!(matches!(
            outbox.get(id).unwrap().state,
            ReplicationState::InProgress { progress: 50 }
        ));

        assert!(outbox.update_progress(id, 100).is_ok());
        assert!(outbox.complete(id).is_ok());
        assert_eq!(outbox.get(id).unwrap().state, ReplicationState::Completed);
    }

    #[test]
    fn test_outbox_failure_flow() {
        let mut outbox = ReplicationOutbox::new();
        let reservation = create_test_reservation();
        let id = outbox.enqueue(reservation);

        assert!(outbox.start(id).is_ok());
        assert!(outbox.fail(id, "network error".to_string()).is_ok());

        let entry = outbox.get(id).unwrap();
        assert!(matches!(entry.state, ReplicationState::Failed { .. }));
        assert_eq!(entry.retry_count, 1);
    }

    #[test]
    fn test_outbox_cancellation() {
        let mut outbox = ReplicationOutbox::new();
        let reservation = create_test_reservation();
        let id = outbox.enqueue(reservation);

        assert!(outbox.start(id).is_ok());
        assert!(outbox.cancel(id).is_ok());
        assert_eq!(outbox.get(id).unwrap().state, ReplicationState::Cancelled);
    }

    #[test]
    fn test_invalid_state_transitions() {
        let mut outbox = ReplicationOutbox::new();
        let reservation = create_test_reservation();
        let id = outbox.enqueue(reservation);

        assert!(outbox.start(id).is_ok());
        assert!(outbox.update_progress(id, 100).is_ok());
        assert!(outbox.complete(id).is_ok());

        assert!(outbox.start(id).is_err());
        assert!(outbox.update_progress(id, 50).is_err());
    }

    #[test]
    fn test_invalid_progress_value() {
        let mut outbox = ReplicationOutbox::new();
        let reservation = create_test_reservation();
        let id = outbox.enqueue(reservation);

        assert!(outbox.start(id).is_ok());
        assert!(outbox.update_progress(id, 101).is_err());
    }

    #[test]
    fn fail_and_cancel_reject_a_completed_entry() {
        let mut outbox = ReplicationOutbox::new();
        let id = outbox.enqueue(create_test_reservation());
        assert!(outbox.start(id).is_ok());
        assert!(outbox.update_progress(id, 100).is_ok());
        assert!(outbox.complete(id).is_ok());

        let fail = outbox.fail(id, "too late".to_string()).unwrap_err();
        let cancel = outbox.cancel(id).unwrap_err();
        assert!(fail.contains("cannot fail"));
        assert!(cancel.contains("cannot cancel"));
        assert_eq!(outbox.get(id).unwrap().state, ReplicationState::Completed);
    }

    #[test]
    fn fail_and_cancel_reject_an_unknown_entry() {
        let mut outbox = ReplicationOutbox::new();
        assert!(outbox.fail(99, "missing".to_string()).is_err());
        assert!(outbox.cancel(99).is_err());
    }
}
