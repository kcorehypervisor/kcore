------------------------------- MODULE ControllerNodeReconcile -------------------------------
EXTENDS Naturals, TLC

CONSTANTS Controllers

ASSUME /\ Controllers # {}
       

VARIABLES Up, Active, HeartbeatCount

Vars == <<Up, Active, HeartbeatCount>>

ReachableControllers(u) == {c \in Controllers : u[c]}

BestReachable(u) ==
  CHOOSE c \in ReachableControllers(u) : TRUE

Init ==
  /\ Up = [c \in Controllers |-> TRUE]
  /\ Active = BestReachable(Up)
  /\ HeartbeatCount = [c \in Controllers |-> 0]

Failover ==
  /\ ~Up[Active]
  /\ ReachableControllers(Up) # {}
  /\ Active' = BestReachable(Up)
  /\ UNCHANGED <<Up, HeartbeatCount>>

Heartbeat ==
  /\ Up[Active]
  /\ HeartbeatCount' = [HeartbeatCount EXCEPT ![Active] = @ + 1]
  /\ UNCHANGED <<Up, Active>>

ToggleReachability ==
  /\ \E c \in Controllers :
      LET newUp == [Up EXCEPT ![c] = ~@]
      IN /\ Up' = newUp
         /\ Active' =
              IF ~newUp[Active] /\ ReachableControllers(newUp) # {}
                THEN BestReachable(newUp)
                ELSE Active
  /\ UNCHANGED <<HeartbeatCount>>

Noop ==
  UNCHANGED Vars

Next ==
  Failover \/ Heartbeat \/ ToggleReachability \/ Noop

Spec ==
  Init /\ [][Next]_Vars
       /\ WF_Vars(Failover)
       /\ WF_Vars(Heartbeat)

TypeOK ==
  /\ Up \in [Controllers -> BOOLEAN]
  /\ Active \in Controllers
  /\ HeartbeatCount \in [Controllers -> Nat]

Safety_ActiveReachableOrNoReachable ==
  Up[Active] \/ ReachableControllers(Up) = {}

Safety_HeartbeatCountersNatural ==
  \A c \in Controllers : HeartbeatCount[c] \in Nat

StateConstraint ==
  \A c \in Controllers : HeartbeatCount[c] <= 2

Liveness_IfAnyReachableThenEventuallyReachableActive ==
  []((ReachableControllers(Up) # {}) => <>Up[Active])

Liveness_HeartbeatsProgressWhenReachable ==
  []((ReachableControllers(Up) # {}) => <>(
      \E c \in Controllers : HeartbeatCount[c] > 0
  ))

==============================================================================================
