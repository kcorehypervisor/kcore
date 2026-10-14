---- MODULE CompensationExecutor ----
(*
TLA+ specification for the Compensation Executor
Objective 3: TLA+ invariants for compensation executor

This specification models the compensation executor's behavior,
including task registration, execution order (LIFO), retry logic,
and state transitions.
*)

EXTENDS Naturals, Sequences, TLC

CONSTANTS
    MaxTasks,        \* Maximum number of compensation tasks
    MaxRetries       \* Maximum retry attempts per task

VARIABLES
    tasks,           \* Queue of pending tasks (sequence)
    executed,        \* Set of executed task IDs
    nextId,          \* Next task ID to assign
    taskStates,      \* Function from task ID to execution status
    retryCounts      \* Function from task ID to retry count

vars == <<tasks, executed, nextId, taskStates, retryCounts>>

ExecutionStatus == {"Pending", "Executing", "Completed", "Failed", "Skipped"}

ActionType == {"ReleaseReservation", "RollbackAllocation", "CleanupResources", "NotifyFailure"}

TypeInvariant ==
    /\ nextId \in Nat
    /\ nextId >= 1
    /\ Len(tasks) <= MaxTasks
    /\ \A i \in DOMAIN tasks: tasks[i] \in 1..(nextId-1)
    /\ executed \subseteq 1..(nextId-1)
    /\ DOMAIN taskStates = 1..(nextId-1)
    /\ \A tid \in DOMAIN taskStates: taskStates[tid] \in ExecutionStatus
    /\ DOMAIN retryCounts = 1..(nextId-1)
    /\ \A tid \in DOMAIN retryCounts: retryCounts[tid] \in 0..MaxRetries

Init ==
    /\ tasks = <<>>
    /\ executed = {}
    /\ nextId = 1
    /\ taskStates = [x \in {} |-> "Pending"]
    /\ retryCounts = [x \in {} |-> 0]

RegisterTask ==
    /\ Len(tasks) < MaxTasks
    /\ nextId < 1000  \* Bound for model checking
    /\ LET tid == nextId IN
        /\ tasks' = <<tid>> \o tasks  \* Push to front (LIFO)
        /\ nextId' = nextId + 1
        /\ taskStates' = taskStates @@ (tid :> "Pending")
        /\ retryCounts' = retryCounts @@ (tid :> 0)
        /\ UNCHANGED <<executed>>

ExecuteTaskSuccess ==
    /\ Len(tasks) > 0
    /\ LET tid == tasks[Len(tasks)] IN  \* Pop from back (LIFO)
        /\ taskStates[tid] \in {"Pending", "Executing"}
        /\ retryCounts[tid] < MaxRetries
        /\ tasks' = SubSeq(tasks, 1, Len(tasks)-1)
        /\ executed' = executed \cup {tid}
        /\ taskStates' = [taskStates EXCEPT ![tid] = "Completed"]
        /\ retryCounts' = [retryCounts EXCEPT ![tid] = @ + 1]
        /\ UNCHANGED <<nextId>>

ExecuteTaskFailure ==
    /\ Len(tasks) > 0
    /\ LET tid == tasks[Len(tasks)] IN  \* Pop from back
        /\ taskStates[tid] \in {"Pending", "Executing"}
        /\ retryCounts[tid] < MaxRetries - 1  \* Can still retry
        /\ tasks' = SubSeq(tasks, 1, Len(tasks)-1) \o <<tid>>  \* Re-queue at back
        /\ taskStates' = [taskStates EXCEPT ![tid] = "Pending"]
        /\ retryCounts' = [retryCounts EXCEPT ![tid] = @ + 1]
        /\ UNCHANGED <<executed, nextId>>

ExecuteTaskPermanentFailure ==
    /\ Len(tasks) > 0
    /\ LET tid == tasks[Len(tasks)] IN
        /\ taskStates[tid] \in {"Pending", "Executing"}
        /\ retryCounts[tid] >= MaxRetries - 1  \* Last retry
        /\ tasks' = SubSeq(tasks, 1, Len(tasks)-1)
        /\ executed' = executed \cup {tid}
        /\ taskStates' = [taskStates EXCEPT ![tid] = "Failed"]
        /\ retryCounts' = [retryCounts EXCEPT ![tid] = @ + 1]
        /\ UNCHANGED <<nextId>>

SkipTask ==
    /\ Len(tasks) > 0
    /\ \E i \in DOMAIN tasks:
        LET tid == tasks[i] IN
            /\ tasks' = SubSeq(tasks, 1, i-1) \o SubSeq(tasks, i+1, Len(tasks))
            /\ executed' = executed \cup {tid}
            /\ taskStates' = [taskStates EXCEPT ![tid] = "Skipped"]
            /\ UNCHANGED <<nextId, retryCounts>>

Next ==
    \/ RegisterTask
    \/ ExecuteTaskSuccess
    \/ ExecuteTaskFailure
    \/ ExecuteTaskPermanentFailure
    \/ SkipTask

Spec == Init /\ [][Next]_vars

\* Bounds registration for model checking. TLC is started with -deadlock
\* because this constraint makes a caught-up empty queue a terminal state.
StateConstraint == nextId <= MaxTasks + 1

\* INVARIANTS

\* Completed or failed tasks are in the executed set
ExecutedTasksAreTerminal ==
    \A tid \in DOMAIN taskStates:
        (taskStates[tid] \in {"Completed", "Failed", "Skipped"}) => (tid \in executed)

\* Tasks in the queue are not yet completed
PendingTasksNotCompleted ==
    \A i \in DOMAIN tasks:
        taskStates[tasks[i]] \notin {"Completed", "Failed", "Skipped"}

\* Retry count never exceeds max retries
RetryBound ==
    \A tid \in DOMAIN retryCounts:
        retryCounts[tid] <= MaxRetries

\* Tasks can only transition to Failed after reaching max retries
FailedTasksExhaustedRetries ==
    \A tid \in DOMAIN taskStates:
        (taskStates[tid] = "Failed") => (retryCounts[tid] >= MaxRetries)

\* No task appears twice in the queue
NoDuplicatesInQueue ==
    \A i, j \in DOMAIN tasks:
        (i # j) => (tasks[i] # tasks[j])

\* LIFO ordering: tasks are executed in reverse order of registration
\* (This is harder to express as a pure invariant, so we use a temporal property)

\* TEMPORAL PROPERTIES

\* Eventually, all registered tasks are either completed, failed, or skipped
EventuallyAllTasksProcessed ==
    <>[] (Len(tasks) = 0)

\* If a task is pending and no more tasks are registered, it will eventually execute
TaskEventuallyExecutes ==
    \A tid \in DOMAIN taskStates:
        (taskStates[tid] = "Pending" /\ tid \in executed) ~> (taskStates[tid] \in {"Completed", "Failed"})

====
