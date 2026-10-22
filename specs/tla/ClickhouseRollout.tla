----------------------------- MODULE ClickhouseRollout -----------------------------
\* Bounded model of a ClickHouse rolling restart.
\*
\* Hardcoded to two shards, two replicas each, and three voting Keepers so
\* TLC finishes inside the formal-checks job. A server restart takes one
\* replica of a shard down only while its partner is up. A Keeper restart
\* takes one voter down only while a strict majority of the other voters
\* are up. FinishServer and FinishKeeper bring that member back, so every
\* state has a successor under TLC -deadlock.
\*
\* The planner step kinds in crates/kcore-clickhouse/src/rollout.rs are:
\*   restart-unit server -> RestartServer, then FinishServer
\*   restart-unit keeper -> RestartKeeper, then FinishKeeper
\*   wait-keeper-quorum  -> the other-voter majority RestartKeeper requires

EXTENDS Naturals, FiniteSets

Shards == {"s1", "s2"}
Replicas == {"r1", "r2"}
Keepers == {"k1", "k2", "k3"}
Status == {"up", "down"}

VARIABLES server, keeper

vars == <<server, keeper>>

Init ==
  /\ server = [s \in Shards |-> [r \in Replicas |-> "up"]]
  /\ keeper = [k \in Keepers |-> "up"]

DownReplicas(s) == {r \in Replicas : server[s][r] = "down"}
UpKeepers == {k \in Keepers : keeper[k] = "up"}

\* Voters that stay up if k is the one taken down.
OtherUp(k) == Cardinality(UpKeepers \ {k})

RestartServer(s, r) ==
  /\ server[s][r] = "up"
  /\ DownReplicas(s) = {}
  /\ server' = [server EXCEPT ![s][r] = "down"]
  /\ UNCHANGED keeper

FinishServer(s, r) ==
  /\ server[s][r] = "down"
  /\ server' = [server EXCEPT ![s][r] = "up"]
  /\ UNCHANGED keeper

RestartKeeper(k) ==
  /\ keeper[k] = "up"
  /\ OtherUp(k) > Cardinality(Keepers) \div 2
  /\ keeper' = [keeper EXCEPT ![k] = "down"]
  /\ UNCHANGED server

FinishKeeper(k) ==
  /\ keeper[k] = "down"
  /\ keeper' = [keeper EXCEPT ![k] = "up"]
  /\ UNCHANGED server

Next ==
  \/ \E s \in Shards, r \in Replicas :
       RestartServer(s, r) \/ FinishServer(s, r)
  \/ \E k \in Keepers :
       RestartKeeper(k) \/ FinishKeeper(k)

Spec == Init /\ [][Next]_vars

TypeOK ==
  /\ server \in [Shards -> [Replicas -> Status]]
  /\ keeper \in [Keepers -> Status]

\* At most one replica of a shard is down, and one replica stays up.
SafetyServers ==
  \A s \in Shards :
    /\ Cardinality(DownReplicas(s)) <= 1
    /\ \E r \in Replicas : server[s][r] = "up"

\* A Keeper that is down still leaves a strict majority of the voters up.
SafetyKeepers ==
  \A k \in Keepers :
    keeper[k] = "down" => 2 * Cardinality(UpKeepers) > Cardinality(Keepers)

Safety == SafetyServers /\ SafetyKeepers

=============================================================================
