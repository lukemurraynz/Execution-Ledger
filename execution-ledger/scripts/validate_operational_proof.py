#!/usr/bin/env python3
"""Validate operational-proof metric snapshots against acceptance gates."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


def load_json(path: Path) -> dict[str, Any]:
    with path.open("r", encoding="utf-8") as file:
        data = json.load(file)
    if not isinstance(data, dict):
        raise ValueError(f"Expected JSON object at root in {path}")
    return data


def get_metric(snapshot: dict[str, Any], dotted_path: str) -> tuple[bool, Any]:
    current: Any = snapshot
    for part in dotted_path.split("."):
        if not isinstance(current, dict) or part not in current:
            return False, None
        current = current[part]
    return True, current


def add_failure(
    failures: list[dict[str, Any]],
    gate: str,
    metric_path: str,
    comparator: str,
    threshold: Any,
    observed: Any,
    reason: str,
) -> None:
    failures.append(
        {
            "gate": gate,
            "metric_path": metric_path,
            "comparator": comparator,
            "threshold": threshold,
            "observed": observed,
            "reason": reason,
            "kill_switch_triggered": True,
        }
    )


def as_int(value: Any) -> int:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"Expected numeric value, got {type(value).__name__}")
    return int(value)


def as_float(value: Any) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise ValueError(f"Expected numeric value, got {type(value).__name__}")
    return float(value)


def evaluate_gates(gates: dict[str, Any], snapshot: dict[str, Any]) -> list[dict[str, Any]]:
    failures: list[dict[str, Any]] = []

    error_budget = gates["error_budget"]
    has, failure_rate = get_metric(snapshot, "error_budget.failure_rate")
    if not has:
        add_failure(failures, "error_budget", "error_budget.failure_rate", "<=", error_budget["max_failure_rate"], None, "Missing metric")
    elif as_float(failure_rate) > as_float(error_budget["max_failure_rate"]):
        add_failure(
            failures,
            "error_budget",
            "error_budget.failure_rate",
            "<=",
            error_budget["max_failure_rate"],
            failure_rate,
            "Failure rate exceeded gate",
        )

    for metric_path, threshold_key in (
        ("error_budget.burn_rate_1h", "max_burn_rate_1h"),
        ("error_budget.burn_rate_6h", "max_burn_rate_6h"),
    ):
        has, observed = get_metric(snapshot, metric_path)
        if not has:
            add_failure(failures, "error_budget", metric_path, "<=", error_budget[threshold_key], None, "Missing metric")
        elif as_float(observed) > as_float(error_budget[threshold_key]):
            add_failure(
                failures,
                "error_budget",
                metric_path,
                "<=",
                error_budget[threshold_key],
                observed,
                "Burn rate exceeded gate",
            )

    has, requests = get_metric(snapshot, "error_budget.total_requests")
    if not has:
        add_failure(
            failures,
            "error_budget",
            "error_budget.total_requests",
            ">=",
            error_budget["min_request_count"],
            None,
            "Missing metric",
        )
    elif as_int(requests) < as_int(error_budget["min_request_count"]):
        add_failure(
            failures,
            "error_budget",
            "error_budget.total_requests",
            ">=",
            error_budget["min_request_count"],
            requests,
            "Insufficient request volume for reliable gate decision",
        )

    queue_lag = gates["queue_lag"]
    for metric_path, threshold_key in (
        ("queue.intent_queue_lag_seconds_p95", "max_intent_queue_lag_p95_seconds"),
        ("queue.intent_queue_oldest_message_seconds", "max_intent_queue_oldest_message_seconds"),
    ):
        has, observed = get_metric(snapshot, metric_path)
        if not has:
            add_failure(failures, "queue_lag", metric_path, "<=", queue_lag[threshold_key], None, "Missing metric")
        elif as_float(observed) > as_float(queue_lag[threshold_key]):
            add_failure(
                failures,
                "queue_lag",
                metric_path,
                "<=",
                queue_lag[threshold_key],
                observed,
                "Queue lag exceeded gate",
            )

    crash_loop = gates["crash_loop"]
    has, restarts = get_metric(snapshot, "runtime.service_restarts")
    if not has:
        add_failure(
            failures,
            "crash_loop",
            "runtime.service_restarts",
            "<=",
            crash_loop["max_total_restarts_in_window"],
            None,
            "Missing metric",
        )
    elif not isinstance(restarts, dict):
        add_failure(
            failures,
            "crash_loop",
            "runtime.service_restarts",
            "dict",
            "service=>count",
            type(restarts).__name__,
            "Invalid metric type",
        )
    else:
        total_restarts = 0
        for service_name, restart_count in restarts.items():
            restart_count_value = as_int(restart_count)
            total_restarts += restart_count_value
            if restart_count_value > as_int(crash_loop["max_restarts_per_service"]):
                add_failure(
                    failures,
                    "crash_loop",
                    f"runtime.service_restarts.{service_name}",
                    "<=",
                    crash_loop["max_restarts_per_service"],
                    restart_count_value,
                    "Service restart count exceeded per-service gate",
                )
        if total_restarts > as_int(crash_loop["max_total_restarts_in_window"]):
            add_failure(
                failures,
                "crash_loop",
                "runtime.service_restarts.total",
                "<=",
                crash_loop["max_total_restarts_in_window"],
                total_restarts,
                "Aggregate restarts exceeded gate",
            )

    security_policy = gates["security_policy"]
    for metric_path, allowed_modes_key in (
        ("security.api_auth_mode", "allowed_api_auth_modes"),
        ("security.servicebus_auth_mode", "allowed_servicebus_auth_modes"),
    ):
        has, mode = get_metric(snapshot, metric_path)
        if not has:
            add_failure(
                failures,
                "security_policy",
                metric_path,
                "in",
                security_policy[allowed_modes_key],
                None,
                "Missing metric",
            )
        elif mode not in security_policy[allowed_modes_key]:
            add_failure(
                failures,
                "security_policy",
                metric_path,
                "in",
                security_policy[allowed_modes_key],
                mode,
                "Disallowed auth mode",
            )

    for metric_path, threshold_key in (
        ("security.secret_resolution_failures", "max_secret_resolution_failures"),
        ("security.missing_required_secrets", "max_missing_required_secrets"),
    ):
        has, observed = get_metric(snapshot, metric_path)
        if not has:
            add_failure(
                failures,
                "security_policy",
                metric_path,
                "<=",
                security_policy[threshold_key],
                None,
                "Missing metric",
            )
        elif as_int(observed) > as_int(security_policy[threshold_key]):
            add_failure(
                failures,
                "security_policy",
                metric_path,
                "<=",
                security_policy[threshold_key],
                observed,
                "Security secret failure gate breached",
            )

    slo_minimums = gates["slo_minimums"]
    for critical_path in slo_minimums["critical_paths"]:
        metric_path = critical_path["metric_path"]
        has, observed = get_metric(snapshot, metric_path)
        if not has:
            add_failure(
                failures,
                "slo_minimums",
                metric_path,
                ">=",
                critical_path["min_value"],
                None,
                "Missing metric",
            )
        elif as_float(observed) < as_float(critical_path["min_value"]):
            add_failure(
                failures,
                "slo_minimums",
                metric_path,
                ">=",
                critical_path["min_value"],
                observed,
                "Critical-path SLO dropped below minimum",
            )

    for latency_path in slo_minimums.get("latency_paths", []):
        metric_path = latency_path["metric_path"]
        has, observed = get_metric(snapshot, metric_path)
        if not has:
            add_failure(
                failures,
                "slo_minimums",
                metric_path,
                "<=",
                latency_path["max_value"],
                None,
                "Missing metric",
            )
        elif as_float(observed) > as_float(latency_path["max_value"]):
            add_failure(
                failures,
                "slo_minimums",
                metric_path,
                "<=",
                latency_path["max_value"],
                observed,
                "Critical-path latency exceeded SLO maximum",
            )

    return failures


def main() -> int:
    parser = argparse.ArgumentParser(description="Validate operational-proof gates against a metrics snapshot.")
    parser.add_argument("--snapshot", required=True, help="Path to metrics snapshot JSON file.")
    parser.add_argument(
        "--gates",
        default="execution-ledger/ops/operational-proof-gates.json",
        help="Path to gate configuration JSON file.",
    )
    args = parser.parse_args()

    gate_file = Path(args.gates)
    snapshot_file = Path(args.snapshot)
    gate_config = load_json(gate_file)
    snapshot = load_json(snapshot_file)

    failures = evaluate_gates(gate_config["gates"], snapshot)
    result = {
        "status": "pass" if not failures else "fail",
        "kill_switch_triggered": bool(failures) and gate_config.get("kill_switch", {}).get("enabled", True),
        "failed_gate_count": len(failures),
        "failures": failures,
    }
    print(json.dumps(result, indent=2, sort_keys=True))
    return 1 if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
