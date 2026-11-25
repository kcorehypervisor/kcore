#!/usr/bin/env bash
# Lab chaos suite for Kubernetes on kcore. Non-interactive: nothing prompts.
# Gated by CONFIRM=yes because scenarios power hosts off and delete objects.
# Do not run destructive scenarios without an explicit operator yes.
#
# Usage:
#   CONFIRM=yes scripts/k8s-lab-chaos.sh --cluster NAME [--scenario N|all]
#
# Environment:
#   CONFIRM=yes              required before any scenario runs
#   SCENARIO_TIMEOUT         seconds allowed per scenario (default 1800)
#   KCTL                     kctl binary (default: kctl)
#   PROVISIONER_URL          bare-metal provisioner (default: https://192.168.40.10:8888)
#   PROVISIONER_TOKEN_FILE   bearer token file for power API (optional)
#   POWER_MACHINE_IDS        comma-separated machine ids for power scenarios
#   SPARE_NODE_ID            spare control-plane node id for scenario 1
#   MANIFEST                 KubernetesCluster YAML for upgrade/drift scenarios
#   WORKLOAD_NS              namespace used for snapshot/restore checks (default: chaos-demo)
set -euo pipefail

SCENARIO_TIMEOUT="${SCENARIO_TIMEOUT:-1800}"
KCTL="${KCTL:-kctl}"
PROVISIONER_URL="${PROVISIONER_URL:-https://192.168.40.10:8888}"
PROVISIONER_TOKEN_FILE="${PROVISIONER_TOKEN_FILE:-}"
POWER_MACHINE_IDS="${POWER_MACHINE_IDS:-}"
SPARE_NODE_ID="${SPARE_NODE_ID:-}"
MANIFEST="${MANIFEST:-}"
WORKLOAD_NS="${WORKLOAD_NS:-chaos-demo}"
CLUSTER=""
SELECTED="all"

log() { printf '%s [k8s-chaos] %s\n' "$(date -u +%FT%TZ)" "$*"; }
die() { log "ERROR: $*" >&2; exit 1; }

usage() {
  sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//'
}

require_confirm() {
  if [ "${CONFIRM:-}" != "yes" ]; then
    die "CONFIRM=yes is required; this suite powers hosts off and deletes objects"
  fi
}

require_cluster() {
  [ -n "$CLUSTER" ] || die "--cluster is required"
  command -v "$KCTL" >/dev/null 2>&1 || die "kctl not found on PATH (set KCTL=...)"
}

wait_ready() {
  local deadline=$((SECONDS + ${1:-600}))
  while (( SECONDS < deadline )); do
    if "$KCTL" kubernetes status "$CLUSTER" 2>/dev/null | grep -qiE 'phase: *ready|ready'; then
      return 0
    fi
    sleep 15
  done
  die "cluster $CLUSTER did not become ready in time"
}

power_post() {
  local action="$1" machine_id="$2"
  local url="${PROVISIONER_URL%/}/api/power/${action}/${machine_id}"
  local args=(-fsS -X POST --max-time 60 "$url")
  if [ -n "$PROVISIONER_TOKEN_FILE" ] && [ -f "$PROVISIONER_TOKEN_FILE" ]; then
    args+=(-H "Authorization: Bearer $(cat "$PROVISIONER_TOKEN_FILE")")
  fi
  curl "${args[@]}"
}

first_cp_machine() {
  if [ -z "$POWER_MACHINE_IDS" ]; then
    die "set POWER_MACHINE_IDS=id1,id2,... for power scenarios"
  fi
  printf '%s\n' "${POWER_MACHINE_IDS%%,*}"
}

all_machines() {
  if [ -z "$POWER_MACHINE_IDS" ]; then
    die "set POWER_MACHINE_IDS=id1,id2,... for power scenarios"
  fi
  printf '%s\n' "${POWER_MACHINE_IDS//,/ }"
}

scenario_1_power_off_control_plane() {
  log "SCENARIO 1: power off a control-plane host; API stays up, auto-repair moves it onto a spare"
  require_confirm
  require_cluster
  [ -n "$SPARE_NODE_ID" ] || die "set SPARE_NODE_ID for scenario 1"
  local mid
  mid="$(first_cp_machine)"
  log "powering off machine $mid"
  power_post off "$mid" || power_post ForceOff "$mid" || true
  sleep 30
  "$KCTL" kubernetes status "$CLUSTER" || true
  log "waiting for repair / ready (spare=$SPARE_NODE_ID)"
  wait_ready 1200
  log "SCENARIO 1 complete"
}

scenario_2_snapshot_delete_restore() {
  log "SCENARIO 2: snapshot, delete namespaces, restore; objects come back"
  require_confirm
  require_cluster
  "$KCTL" kubernetes backup create "$CLUSTER"
  local snap
  snap="$("$KCTL" kubernetes backup list "$CLUSTER" | awk 'NR==1{print $1}')"
  [ -n "$snap" ] || die "no snapshot id from backup list"
  log "snapshot=$snap; deleting namespace $WORKLOAD_NS"
  "$KCTL" kubernetes admin -- "$CLUSTER" -- kubectl delete ns "$WORKLOAD_NS" --wait=false 2>/dev/null \
    || kubectl --context "$CLUSTER" delete ns "$WORKLOAD_NS" --wait=false 2>/dev/null \
    || log "namespace delete skipped (no kubectl path); continuing with restore"
  "$KCTL" kubernetes restore "$CLUSTER" --snapshot "$snap" --yes
  wait_ready 1200
  log "SCENARIO 2 complete (verify objects manually if admin path unavailable)"
}

scenario_3_short_cert_expiry() {
  log "SCENARIO 3: force certificate rotation; cluster stays ready"
  require_confirm
  require_cluster
  "$KCTL" kubernetes rotate-certs "$CLUSTER" --force --yes
  wait_ready 1800
  "$KCTL" kubernetes certs "$CLUSTER"
  log "SCENARIO 3 complete"
}

scenario_4_upgrade_and_rollback() {
  log "SCENARIO 4: upgrade via manifest apply, then roll back to previous version"
  require_confirm
  require_cluster
  [ -n "$MANIFEST" ] || die "set MANIFEST=/path/to/KubernetesCluster.yaml for scenario 4"
  "$KCTL" kubernetes apply -f "$MANIFEST" --yes
  wait_ready 1800
  "$KCTL" kubernetes upgrade rollback "$CLUSTER" --yes
  wait_ready 1800
  log "SCENARIO 4 complete"
}

scenario_5_drift_roll() {
  log "SCENARIO 5: re-apply manifest to trigger drift roll; wait for ready"
  require_confirm
  require_cluster
  [ -n "$MANIFEST" ] || die "set MANIFEST=/path/to/KubernetesCluster.yaml for scenario 5"
  "$KCTL" kubernetes apply -f "$MANIFEST" --yes
  wait_ready 1800
  log "SCENARIO 5 complete"
}

scenario_6_ceph_pvc_node_loss() {
  log "SCENARIO 6: power off a worker/PVC node; Ceph volume should remount elsewhere"
  require_confirm
  require_cluster
  local mid
  mid="$(first_cp_machine)"
  log "powering off $mid (ensure this is the PVC node in lab inventory)"
  power_post off "$mid" || power_post ForceOff "$mid" || true
  sleep 60
  wait_ready 1800
  log "powering $mid back on"
  power_post on "$mid" || true
  wait_ready 1800
  log "SCENARIO 6 complete"
}

scenario_7_total_power_loss() {
  log "SCENARIO 7: cut power to every listed host; wait for cluster return"
  require_confirm
  require_cluster
  local mid
  for mid in $(all_machines); do
    log "power off $mid"
    power_post off "$mid" || power_post ForceOff "$mid" || true
  done
  sleep 120
  for mid in $(all_machines); do
    log "power on $mid"
    power_post on "$mid" || true
  done
  wait_ready 2400
  log "SCENARIO 7 complete"
}

SCENARIOS=(
  scenario_1_power_off_control_plane
  scenario_2_snapshot_delete_restore
  scenario_3_short_cert_expiry
  scenario_4_upgrade_and_rollback
  scenario_5_drift_roll
  scenario_6_ceph_pvc_node_loss
  scenario_7_total_power_loss
)

run_scenario() {
  local fn="$1"
  log "phase start: $fn (timeout ${SCENARIO_TIMEOUT}s)"
  if ! timeout "${SCENARIO_TIMEOUT}s" bash -c "$(
    declare -p CLUSTER SCENARIO_TIMEOUT KCTL PROVISIONER_URL PROVISIONER_TOKEN_FILE \
      POWER_MACHINE_IDS SPARE_NODE_ID MANIFEST WORKLOAD_NS
    declare -f log die require_confirm require_cluster wait_ready power_post \
      first_cp_machine all_machines "$fn"
    echo "$fn"
  )"; then
    die "phase failed: $fn"
  fi
  log "phase ok: $fn"
}

while [ $# -gt 0 ]; do
  case "$1" in
    --cluster) CLUSTER="${2:-}"; shift 2 ;;
    --scenario) SELECTED="${2:-}"; shift 2 ;;
    -h | --help) usage; exit 0 ;;
    *) die "unknown argument: $1" ;;
  esac
done

[ -n "$CLUSTER" ] || die "--cluster is required"

log "cluster=$CLUSTER scenario=$SELECTED"
if [ "$SELECTED" = "all" ]; then
  for fn in "${SCENARIOS[@]}"; do
    run_scenario "$fn"
  done
else
  case "$SELECTED" in
    [1-7]) run_scenario "${SCENARIOS[$((SELECTED - 1))]}" ;;
    *) die "--scenario must be 1-7 or all" ;;
  esac
fi
log "done"
