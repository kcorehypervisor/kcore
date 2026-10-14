---- MODULE DiskLayoutReconcile ----
(*
Per-resource state machine for a controller-orchestrated DiskLayout.

The node may enter Applying or Applied only after the safe/dangerous
classifier has approved the generation. A refusal or a later revision clears
that approval before another apply can start.
*)

EXTENDS Naturals

CONSTANT MaxGen

VARIABLES phase, approved, generation, observed

vars == <<phase, approved, generation, observed>>

Phase == {"Pending", "Classifying", "Refused", "Approved", "Applying", "Applied"}

TypeOK ==
    /\ phase \in Phase
    /\ approved \in BOOLEAN
    /\ generation \in 1..MaxGen
    /\ observed \in 0..MaxGen
    /\ observed <= generation

Init ==
    /\ phase = "Pending"
    /\ approved = FALSE
    /\ generation = 1
    /\ observed = 0

Classify ==
    /\ phase = "Pending"
    /\ phase' = "Classifying"
    /\ UNCHANGED <<approved, generation, observed>>

Refuse ==
    /\ phase = "Classifying"
    /\ phase' = "Refused"
    /\ approved' = FALSE
    /\ UNCHANGED <<generation, observed>>

Approve ==
    /\ phase = "Classifying"
    /\ phase' = "Approved"
    /\ approved' = TRUE
    /\ UNCHANGED <<generation, observed>>

StartApply ==
    /\ phase = "Approved"
    /\ approved = TRUE
    /\ phase' = "Applying"
    /\ UNCHANGED <<approved, generation, observed>>

FinishApply ==
    /\ phase = "Applying"
    /\ approved = TRUE
    /\ phase' = "Applied"
    /\ observed' = generation
    /\ UNCHANGED <<approved, generation>>

Revise ==
    /\ phase \in {"Refused", "Applied"}
    /\ generation < MaxGen
    /\ phase' = "Pending"
    /\ approved' = FALSE
    /\ generation' = generation + 1
    /\ UNCHANGED observed

Terminal ==
    /\ phase \in {"Refused", "Applied"}
    /\ generation = MaxGen
    /\ UNCHANGED vars

Next ==
    \/ Classify
    \/ Refuse
    \/ Approve
    \/ StartApply
    \/ FinishApply
    \/ Revise
    \/ Terminal

Spec == Init /\ [][Next]_vars

\* No Applied (or in-flight apply) without classifier approval.
NoApplyWithoutApproval ==
    phase \in {"Approved", "Applying", "Applied"} => approved

AppliedMatchesGeneration ==
    phase = "Applied" => observed = generation

RefusedIsNotApproved ==
    phase \in {"Pending", "Classifying", "Refused"} => ~approved

====
