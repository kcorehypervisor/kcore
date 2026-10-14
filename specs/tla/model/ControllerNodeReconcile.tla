---- MODULE ControllerNodeReconcile ----
(*
Controller to node-agent reconciliation.

The controller publishes a desired generation per node. A node applies that
generation only while it is rebuilding. A crash mid-apply leaves the previous
generation in place and marks the node unreachable, so a ready node is never
advertising a stale config.
*)

EXTENDS Naturals

CONSTANT MaxGen

Nodes == 1..2

VARIABLES desired, applied, status, inflight

vars == <<desired, applied, status, inflight>>

NodeStatus == {"unknown", "ready", "rebuilding", "unreachable"}

TypeOK ==
    /\ desired \in [Nodes -> 0..MaxGen]
    /\ applied \in [Nodes -> 0..MaxGen]
    /\ status \in [Nodes -> NodeStatus]
    /\ inflight \in [Nodes -> BOOLEAN]

Init ==
    /\ desired = [n \in Nodes |-> 0]
    /\ applied = [n \in Nodes |-> 0]
    /\ status = [n \in Nodes |-> "unknown"]
    /\ inflight = [n \in Nodes |-> FALSE]

\* Publishing a new generation drops a converged node out of ready so it
\* cannot keep advertising a stale config.
BumpDesired(n) ==
    /\ desired[n] < MaxGen
    /\ desired' = [desired EXCEPT ![n] = @ + 1]
    /\ status' = [status EXCEPT ![n] =
                    IF status[n] = "ready" THEN "unknown" ELSE status[n]]
    /\ UNCHANGED <<applied, inflight>>

StartApply(n) ==
    /\ applied[n] < desired[n]
    /\ status[n] \in {"unknown", "ready"}
    /\ inflight[n] = FALSE
    /\ status' = [status EXCEPT ![n] = "rebuilding"]
    /\ inflight' = [inflight EXCEPT ![n] = TRUE]
    /\ UNCHANGED <<desired, applied>>

FinishApply(n) ==
    /\ status[n] = "rebuilding"
    /\ inflight[n] = TRUE
    /\ applied' = [applied EXCEPT ![n] = desired[n]]
    /\ status' = [status EXCEPT ![n] = "ready"]
    /\ inflight' = [inflight EXCEPT ![n] = FALSE]
    /\ UNCHANGED desired

CrashDuringApply(n) ==
    /\ status[n] = "rebuilding"
    /\ inflight[n] = TRUE
    /\ status' = [status EXCEPT ![n] = "unreachable"]
    /\ inflight' = [inflight EXCEPT ![n] = FALSE]
    /\ UNCHANGED <<desired, applied>>

Recover(n) ==
    /\ status[n] = "unreachable"
    /\ inflight[n] = FALSE
    /\ status' = [status EXCEPT ![n] = "unknown"]
    /\ UNCHANGED <<desired, applied, inflight>>

\* Lets TLC terminate once every node is caught up at the generation bound.
Quiescent ==
    /\ \A n \in Nodes:
        /\ desired[n] = MaxGen
        /\ applied[n] = desired[n]
        /\ status[n] \in {"ready", "unknown"}
        /\ inflight[n] = FALSE
    /\ UNCHANGED vars

Next ==
    \/ \E n \in Nodes:
        \/ BumpDesired(n)
        \/ StartApply(n)
        \/ FinishApply(n)
        \/ CrashDuringApply(n)
        \/ Recover(n)
    \/ Quiescent

Spec == Init /\ [][Next]_vars

AppliedNeverExceedsDesired ==
    \A n \in Nodes: applied[n] <= desired[n]

\* A ready node has the generation the controller last published.
ReadyMeansConverged ==
    \A n \in Nodes: status[n] = "ready" => applied[n] = desired[n]

InflightIffRebuilding ==
    \A n \in Nodes: inflight[n] <=> (status[n] = "rebuilding")

\* A crash cannot publish a partial apply: rebuilding always means the node
\* is still behind the desired generation.
RebuildingIsBehind ==
    \A n \in Nodes: status[n] = "rebuilding" => applied[n] < desired[n]

====
