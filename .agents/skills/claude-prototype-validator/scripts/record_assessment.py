#!/usr/bin/env python3
"""Record the main agent's semantic purpose-gate decision for one prototype."""

from __future__ import annotations

import argparse
import json
import os
import sys
import tempfile
from pathlib import Path
from typing import Any


def nonempty(value: str, label: str) -> str:
    text = value.strip()
    if not text:
        raise ValueError(f"{label} must not be empty")
    return text


def atomic_write(path: Path, payload: dict[str, Any]) -> None:
    descriptor, temp_name = tempfile.mkstemp(prefix=f".{path.name}.", dir=path.parent)
    try:
        with os.fdopen(descriptor, "w", encoding="utf-8") as handle:
            json.dump(payload, handle, ensure_ascii=False, indent=2)
            handle.write("\n")
        os.replace(temp_name, path)
    except Exception:
        try:
            os.unlink(temp_name)
        except FileNotFoundError:
            pass
        raise


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Record a main-agent prototype purpose-gate assessment"
    )
    parser.add_argument("--result", required=True, help="Variant result.json")
    parser.add_argument(
        "--verdict", required=True, choices=["yes", "no", "indeterminate"]
    )
    parser.add_argument(
        "--rationale",
        required=True,
        help="Why the prototype does or does not answer the original question",
    )
    parser.add_argument(
        "--evidence",
        action="append",
        default=[],
        help="Concrete artifact or observation; repeat as needed",
    )
    parser.add_argument(
        "--missing-evidence",
        action="append",
        default=[],
        help="Evidence still needed; repeat as needed",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()
    try:
        result_path = Path(args.result).expanduser().resolve()
        payload = json.loads(result_path.read_text(encoding="utf-8"))
        if not isinstance(payload, dict):
            raise ValueError("result.json must contain an object")
        mechanical_passed = payload.get("mechanical_gate", {}).get("passed") is True
        rationale = nonempty(args.rationale, "rationale")
        evidence = [nonempty(item, "evidence") for item in args.evidence]
        missing = [nonempty(item, "missing-evidence") for item in args.missing_evidence]
        if args.verdict == "yes":
            if not mechanical_passed:
                raise ValueError("verdict yes requires mechanical_gate.passed=true")
            if not evidence:
                raise ValueError("verdict yes requires at least one --evidence item")
            if missing:
                raise ValueError("verdict yes cannot contain missing evidence")
        if args.verdict == "indeterminate" and not missing:
            raise ValueError(
                "verdict indeterminate requires at least one --missing-evidence item"
            )

        assessment = {
            "verdict": args.verdict,
            "rationale": rationale,
            "evidence": evidence,
            "missing_evidence": missing,
            "recorded_by": "main_agent",
        }
        payload["purpose_gate"] = assessment
        atomic_write(result_path, payload)
        atomic_write(result_path.with_name("purpose-assessment.json"), assessment)
        print(json.dumps(assessment, ensure_ascii=False, indent=2))
        return 0
    except (OSError, json.JSONDecodeError, ValueError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
