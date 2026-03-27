#!/usr/bin/env python3
import json
import sys
from pathlib import Path

AUTO_TERMINALS = {"auto_accepted", "auto_rejected", "auto_compensated", "none"}


def winner(current, challenger):
    if current is None:
        return challenger
    if challenger["rank"] > current["rank"]:
        return challenger
    if challenger["rank"] < current["rank"]:
        return current
    return challenger if challenger["op_id"] > current["op_id"] else current


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: check-replication-trace.py <trace.json>")
        return 2

    trace_path = Path(sys.argv[1])
    events = json.loads(trace_path.read_text(encoding="utf-8"))
    if not isinstance(events, list):
        print("trace root must be a JSON array")
        return 2

    heads = {}
    errors = []

    for idx, raw in enumerate(events):
        required = {"resource_key", "op_id", "rank", "terminal_state"}
        missing = required.difference(raw.keys())
        if missing:
            errors.append(f"event[{idx}] missing keys: {sorted(missing)}")
            continue

        event = {
            "resource_key": str(raw["resource_key"]),
            "op_id": str(raw["op_id"]),
            "rank": int(raw["rank"]),
            "terminal_state": str(raw["terminal_state"]),
        }

        if event["terminal_state"] not in AUTO_TERMINALS:
            errors.append(
                f"event[{idx}] has non-auto terminal_state={event['terminal_state']}"
            )

        key = event["resource_key"]
        prior = heads.get(key)
        new_head = winner(prior, event)
        heads[key] = new_head

        expected = raw.get("expected_winner_op_id")
        if expected is not None and str(expected) != new_head["op_id"]:
            errors.append(
                f"event[{idx}] expected winner {expected}, got {new_head['op_id']}"
            )

    if errors:
        print("trace check failed:")
        for err in errors:
            print(f"  - {err}")
        return 1

    print("trace check passed.")
    for resource_key in sorted(heads):
        head = heads[resource_key]
        print(f"  {resource_key}: winner={head['op_id']} rank={head['rank']}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
