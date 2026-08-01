#!/usr/bin/env python3
"""Run bounded Claude Code prototype experiments in isolated local clones."""

from __future__ import annotations

import argparse
import concurrent.futures
import fnmatch
import hashlib
import json
import os
import re
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Any


WORKER_SCHEMA: dict[str, Any] = {
    "type": "object",
    "additionalProperties": False,
    "required": [
        "status",
        "summary",
        "implemented_steps",
        "claimed_changed_files",
        "observations",
        "open_issues",
    ],
    "properties": {
        "status": {"type": "string", "enum": ["completed", "blocked", "failed"]},
        "summary": {"type": "string"},
        "implemented_steps": {"type": "array", "items": {"type": "string"}},
        "claimed_changed_files": {"type": "array", "items": {"type": "string"}},
        "observations": {"type": "array", "items": {"type": "string"}},
        "open_issues": {"type": "array", "items": {"type": "string"}},
    },
}

DEFAULT_TOOLS = ["Read", "Glob", "Grep", "LS", "Edit", "Write"]
ID_PATTERN = re.compile(r"^[A-Za-z0-9._-]+$")


class PrototypeError(RuntimeError):
    pass


@dataclass(frozen=True)
class RunConfig:
    repo: Path
    output_dir: Path
    claude_bin: str
    model: str | None
    effort: str
    max_turns: int
    max_budget_usd: float
    timeout: int
    validation_timeout: int
    include_working_tree: bool
    include_untracked: tuple[str, ...]
    tools: tuple[str, ...]
    allowed_tools: tuple[str, ...]
    validation_pass_env: tuple[str, ...]
    prepare_only: bool


def run_command(
    args: list[str],
    *,
    cwd: Path,
    input_text: str | None = None,
    timeout: int | None = None,
    check: bool = False,
) -> subprocess.CompletedProcess[str]:
    try:
        completed = subprocess.run(
            args,
            cwd=str(cwd),
            input=input_text,
            capture_output=True,
            text=True,
            timeout=timeout,
            check=False,
        )
    except subprocess.TimeoutExpired as exc:
        raise PrototypeError(
            f"Command timed out after {timeout}s: {' '.join(args)}"
        ) from exc
    if check and completed.returncode != 0:
        detail = completed.stderr.strip() or completed.stdout.strip()
        raise PrototypeError(
            f"Command failed ({completed.returncode}): {' '.join(args)}\n{detail}"
        )
    return completed


def require_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value.strip():
        raise PrototypeError(f"Contract field `{field}` must be a non-empty string")
    return value.strip()


def require_string_list(value: Any, field: str) -> list[str]:
    if not isinstance(value, list) or not value:
        raise PrototypeError(f"Contract field `{field}` must be a non-empty array")
    result = [require_string(item, f"{field}[]") for item in value]
    return result


def validate_contract(payload: Any) -> dict[str, Any]:
    if not isinstance(payload, dict):
        raise PrototypeError("Contract must be a JSON object")
    if payload.get("version") != 1:
        raise PrototypeError("Contract `version` must be 1")

    for field in ("experiment", "question", "purpose", "why_prototype"):
        require_string(payload.get(field), field)
    experiment = str(payload["experiment"])
    if not ID_PATTERN.fullmatch(experiment):
        raise PrototypeError(
            "Contract `experiment` may contain only letters, numbers, dot, underscore, and dash"
        )
    for field in ("baseline", "shared_constraints", "evidence_required", "non_goals"):
        require_string_list(payload.get(field), field)

    variants = payload.get("variants")
    if not isinstance(variants, list) or not variants:
        raise PrototypeError("Contract `variants` must be a non-empty array")
    seen: set[str] = set()
    for index, variant in enumerate(variants):
        prefix = f"variants[{index}]"
        if not isinstance(variant, dict):
            raise PrototypeError(f"{prefix} must be an object")
        variant_id = require_string(variant.get("id"), f"{prefix}.id")
        if not ID_PATTERN.fullmatch(variant_id):
            raise PrototypeError(f"{prefix}.id contains unsupported characters")
        if variant_id in seen:
            raise PrototypeError(f"Duplicate variant id: {variant_id}")
        seen.add(variant_id)
        require_string(variant.get("design"), f"{prefix}.design")
        for field in (
            "implementation_plan",
            "allowed_scope",
            "forbidden_scope",
            "success_criteria",
            "validation_commands",
        ):
            require_string_list(variant.get(field), f"{prefix}.{field}")
        for field in ("allowed_scope", "forbidden_scope"):
            for rule in variant[field]:
                rule_path = Path(rule)
                if rule_path.is_absolute() or ".." in rule_path.parts:
                    raise PrototypeError(
                        f"{prefix}.{field} contains an unsafe path rule: {rule}"
                    )
    return payload


def safe_relative_path(repo: Path, raw: str) -> tuple[Path, Path]:
    relative = Path(raw)
    if relative.is_absolute() or ".." in relative.parts or ".git" in relative.parts:
        raise PrototypeError(f"Unsafe untracked path: {raw}")
    source = (repo / relative).resolve()
    try:
        source.relative_to(repo)
    except ValueError as exc:
        raise PrototypeError(f"Untracked path escapes repository: {raw}") from exc
    if not source.exists():
        raise PrototypeError(f"Untracked path does not exist: {raw}")
    candidates = [source]
    if source.is_dir():
        candidates.extend(source.rglob("*"))
    for candidate in candidates:
        if candidate.is_symlink():
            raise PrototypeError(
                f"Refusing to copy symlink from untracked path: {candidate}"
            )
    return relative, source


def copy_explicit_untracked(
    repo: Path, workspace: Path, paths: tuple[str, ...]
) -> None:
    for raw in paths:
        relative, source = safe_relative_path(repo, raw)
        destination = workspace / relative
        destination.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, destination, dirs_exist_ok=True)
        else:
            shutil.copy2(source, destination)


def prepare_workspace(config: RunConfig, variant_dir: Path) -> tuple[Path, str]:
    workspace = variant_dir / "workspace"
    run_command(
        [
            "git",
            "clone",
            "--local",
            "--no-hardlinks",
            "--quiet",
            str(config.repo),
            str(workspace),
        ],
        cwd=config.output_dir,
        check=True,
    )
    if config.include_working_tree:
        patch = run_command(
            ["git", "diff", "--binary", "HEAD"], cwd=config.repo, check=True
        ).stdout
        if patch:
            run_command(
                ["git", "apply", "--binary", "-"],
                cwd=workspace,
                input_text=patch,
                check=True,
            )
    copy_explicit_untracked(config.repo, workspace, config.include_untracked)
    status = run_command(
        ["git", "status", "--porcelain"], cwd=workspace, check=True
    ).stdout
    if status.strip() or config.include_untracked:
        run_command(["git", "add", "-A"], cwd=workspace, check=True)
        if config.include_untracked:
            run_command(
                ["git", "add", "-f", "--", *config.include_untracked],
                cwd=workspace,
                check=True,
            )
        run_command(
            [
                "git",
                "-c",
                "user.name=claude-prototype-validator",
                "-c",
                "user.email=prototype-validator@example.invalid",
                "-c",
                "commit.gpgsign=false",
                "commit",
                "--quiet",
                "-m",
                "prototype baseline snapshot",
            ],
            cwd=workspace,
            check=True,
        )
    baseline = run_command(
        ["git", "rev-parse", "HEAD"], cwd=workspace, check=True
    ).stdout.strip()
    return workspace, baseline


def build_prompt(contract: dict[str, Any], variant: dict[str, Any]) -> str:
    shared = {key: value for key, value in contract.items() if key != "variants"}
    return f"""You are Claude Code acting as a bounded prototype implementation worker.

This is a disposable experiment, not a production implementation. The main Codex agent has already designed the experiment and remains responsible for deciding whether the prototype answers the design question.

Rules:
- Implement exactly the selected variant and follow its ordered implementation plan.
- Make the minimum changes needed to produce the requested evidence.
- Stay inside allowed_scope and never edit forbidden_scope.
- Do not redesign the experiment, substitute another architecture, commit, push, or contact the user.
- Do not access files outside this workspace.
- Do not claim that a command ran unless you actually ran it. The supervisor will independently run contract validation commands afterward.
- If the plan cannot be implemented faithfully, return blocked with the exact reason instead of improvising.
- Return only output matching the required JSON schema.

Shared experiment contract:
{json.dumps(shared, ensure_ascii=False, indent=2)}

Selected variant:
{json.dumps(variant, ensure_ascii=False, indent=2)}
"""


def terminate_process(process: subprocess.Popen[str]) -> None:
    if process.poll() is not None:
        return
    try:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=5)
    except (ProcessLookupError, subprocess.TimeoutExpired):
        try:
            os.killpg(process.pid, signal.SIGKILL)
        except ProcessLookupError:
            pass


def invoke_claude(
    config: RunConfig, workspace: Path, prompt: str
) -> tuple[int, str, str, bool, float]:
    args = [
        config.claude_bin,
        "--safe-mode",
        "-p",
        prompt,
        "--output-format",
        "json",
        "--json-schema",
        json.dumps(WORKER_SCHEMA, separators=(",", ":")),
        "--no-session-persistence",
        "--permission-mode",
        "acceptEdits",
        "--tools",
        ",".join(config.tools),
        "--effort",
        config.effort,
        "--max-turns",
        str(config.max_turns),
        "--max-budget-usd",
        str(config.max_budget_usd),
    ]
    if config.allowed_tools:
        args.extend(["--allowedTools", ",".join(config.allowed_tools)])
    if config.model:
        args.extend(["--model", config.model])

    started = time.monotonic()
    process = subprocess.Popen(
        args,
        cwd=str(workspace),
        stdin=subprocess.DEVNULL,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    timed_out = False
    try:
        stdout, stderr = process.communicate(timeout=config.timeout)
    except subprocess.TimeoutExpired:
        timed_out = True
        terminate_process(process)
        stdout, stderr = process.communicate()
    elapsed = round(time.monotonic() - started, 3)
    return (
        124 if timed_out else int(process.returncode or 0),
        stdout,
        stderr,
        timed_out,
        elapsed,
    )


def parse_worker_output(
    stdout: str,
) -> tuple[dict[str, Any] | None, dict[str, Any] | None, str | None]:
    try:
        envelope = json.loads(stdout)
    except json.JSONDecodeError as exc:
        return None, None, f"Claude output was not valid JSON: {exc}"
    if not isinstance(envelope, dict):
        return None, None, "Claude JSON envelope was not an object"
    if envelope.get("is_error") is True or envelope.get("subtype") not in {
        None,
        "success",
    }:
        return envelope, None, "Claude envelope reported a non-success result"
    structured = envelope.get("structured_output")
    if not isinstance(structured, dict):
        return envelope, None, "Claude envelope did not contain structured_output"
    missing = [key for key in WORKER_SCHEMA["required"] if key not in structured]
    if missing:
        return (
            envelope,
            structured,
            f"Claude structured output missed fields: {', '.join(missing)}",
        )
    if structured.get("status") not in {"completed", "blocked", "failed"}:
        return (
            envelope,
            structured,
            "Claude structured output used an unsupported status",
        )
    for field in ("summary",):
        if not isinstance(structured.get(field), str):
            return (
                envelope,
                structured,
                f"Claude structured output field `{field}` was not a string",
            )
    for field in (
        "implemented_steps",
        "claimed_changed_files",
        "observations",
        "open_issues",
    ):
        if not isinstance(structured.get(field), list) or not all(
            isinstance(item, str) for item in structured[field]
        ):
            return (
                envelope,
                structured,
                f"Claude structured output field `{field}` was not a string array",
            )
    return envelope, structured, None


def nul_paths(output: str) -> list[str]:
    return [item for item in output.split("\0") if item]


def changed_paths(workspace: Path) -> list[str]:
    tracked = nul_paths(
        run_command(
            ["git", "diff", "--name-only", "-z", "HEAD"], cwd=workspace, check=True
        ).stdout
    )
    untracked = nul_paths(
        run_command(
            ["git", "ls-files", "--others", "--exclude-standard", "-z"],
            cwd=workspace,
            check=True,
        ).stdout
    )
    return sorted(set(tracked + untracked))


def file_manifest(workspace: Path, paths: list[str]) -> list[dict[str, Any]]:
    manifest: list[dict[str, Any]] = []
    for relative in paths:
        path = workspace / relative
        if not path.exists():
            manifest.append({"path": relative, "state": "deleted"})
            continue
        if path.is_dir():
            manifest.append({"path": relative, "state": "directory"})
            continue
        data = path.read_bytes()
        manifest.append(
            {
                "path": relative,
                "state": "file",
                "bytes": len(data),
                "sha256": hashlib.sha256(data).hexdigest(),
            }
        )
    return manifest


def write_complete_diff(workspace: Path, destination: Path, paths: list[str]) -> None:
    untracked = nul_paths(
        run_command(
            ["git", "ls-files", "--others", "--exclude-standard", "-z"],
            cwd=workspace,
            check=True,
        ).stdout
    )
    if untracked:
        run_command(["git", "add", "-N", "--", *untracked], cwd=workspace, check=True)
    diff = (
        run_command(
            ["git", "diff", "--binary", "--no-ext-diff", "HEAD", "--", *paths],
            cwd=workspace,
            check=True,
        ).stdout
        if paths
        else ""
    )
    destination.write_text(diff, encoding="utf-8")


def scope_matches(path: str, rule: str) -> bool:
    normalized = rule.strip()
    while normalized.startswith("./"):
        normalized = normalized[2:]
    if normalized.endswith("/"):
        return path.startswith(normalized)
    if any(character in normalized for character in "*?["):
        return fnmatch.fnmatchcase(path, normalized)
    return path == normalized


def scope_report(paths: list[str], variant: dict[str, Any]) -> dict[str, Any]:
    allowed = variant["allowed_scope"]
    forbidden = variant["forbidden_scope"]
    outside = [
        path for path in paths if not any(scope_matches(path, rule) for rule in allowed)
    ]
    forbidden_hits = [
        path for path in paths if any(scope_matches(path, rule) for rule in forbidden)
    ]
    return {
        "allowed_scope": allowed,
        "forbidden_scope": forbidden,
        "outside_allowed_scope": outside,
        "forbidden_scope_hits": forbidden_hits,
        "passed": not outside and not forbidden_hits,
    }


def validation_environment(
    workspace: Path, pass_names: tuple[str, ...]
) -> dict[str, str]:
    home = workspace.parent / "validation-home"
    cache = home / ".cache"
    config = home / ".config"
    cache.mkdir(parents=True, exist_ok=True)
    config.mkdir(parents=True, exist_ok=True)
    environment = {
        "PATH": os.environ.get("PATH", "/usr/bin:/bin"),
        "HOME": str(home),
        "XDG_CACHE_HOME": str(cache),
        "XDG_CONFIG_HOME": str(config),
        "CI": "1",
        "GIT_TERMINAL_PROMPT": "0",
        "PYTHONDONTWRITEBYTECODE": "1",
    }
    for name in ("LANG", "LC_ALL", "TZ"):
        if name in os.environ:
            environment[name] = os.environ[name]
    for name in pass_names:
        if name in os.environ:
            environment[name] = os.environ[name]
    return environment


def run_validations(
    workspace: Path,
    commands: list[str],
    timeout: int,
    pass_env: tuple[str, ...],
) -> list[dict[str, Any]]:
    results: list[dict[str, Any]] = []
    environment = validation_environment(workspace, pass_env)
    for command in commands:
        started = time.monotonic()
        try:
            completed = subprocess.run(
                ["bash", "-lc", command],
                cwd=str(workspace),
                stdin=subprocess.DEVNULL,
                capture_output=True,
                text=True,
                timeout=timeout,
                check=False,
                env=environment,
            )
            results.append(
                {
                    "command": command,
                    "status": "passed" if completed.returncode == 0 else "failed",
                    "returncode": completed.returncode,
                    "elapsed_seconds": round(time.monotonic() - started, 3),
                    "stdout": completed.stdout,
                    "stderr": completed.stderr,
                }
            )
        except subprocess.TimeoutExpired as exc:
            results.append(
                {
                    "command": command,
                    "status": "timeout",
                    "returncode": None,
                    "elapsed_seconds": round(time.monotonic() - started, 3),
                    "stdout": exc.stdout or "",
                    "stderr": exc.stderr or "",
                }
            )
    return results


def run_variant(
    contract: dict[str, Any], variant: dict[str, Any], config: RunConfig
) -> dict[str, Any]:
    variant_id = variant["id"]
    variant_dir = config.output_dir / "variants" / variant_id
    variant_dir.mkdir(parents=True, exist_ok=False)
    workspace, baseline_commit = prepare_workspace(config, variant_dir)
    prompt = build_prompt(contract, variant)
    (variant_dir / "prompt.md").write_text(prompt, encoding="utf-8")
    (variant_dir / "contract.json").write_text(
        json.dumps({**contract, "variants": [variant]}, ensure_ascii=False, indent=2)
        + "\n",
        encoding="utf-8",
    )

    if config.prepare_only:
        result = {
            "variant": variant_id,
            "workspace": str(workspace),
            "baseline_commit": baseline_commit,
            "isolation": {
                "disposable_git_clone": True,
                "main_worktree_write_isolated": True,
                "os_filesystem_sandbox": False,
                "claude_read_boundary": "Claude permission system; not an OS boundary",
                "write_scope_enforcement": "post-run detection inside disposable clone",
                "validation_commands_os_sandboxed": False,
            },
            "state": "prepared",
            "mechanical_gate": {
                "passed": False,
                "reason": "prepare-only; Claude was not invoked",
            },
            "purpose_gate": {
                "verdict": "pending_main_review",
                "rationale": "",
                "evidence": [],
                "missing_evidence": [],
            },
        }
        (variant_dir / "result.json").write_text(
            json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        return result

    returncode, stdout, stderr, timed_out, elapsed = invoke_claude(
        config, workspace, prompt
    )
    (variant_dir / "claude-stdout.json").write_text(stdout, encoding="utf-8")
    (variant_dir / "claude-stderr.log").write_text(stderr, encoding="utf-8")
    envelope, structured, parse_error = parse_worker_output(stdout)

    before_validation = changed_paths(workspace)
    manifest = file_manifest(workspace, before_validation)
    scope = scope_report(before_validation, variant)
    (variant_dir / "changed-files.json").write_text(
        json.dumps(manifest, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    write_complete_diff(workspace, variant_dir / "diff.patch", before_validation)

    worker_passed = (
        returncode == 0
        and parse_error is None
        and structured is not None
        and structured.get("status") == "completed"
    )
    validation_results: list[dict[str, Any]] = []
    validation_skip_reason: str | None = None
    if not worker_passed:
        validation_skip_reason = "worker did not complete successfully"
    elif not scope["passed"]:
        validation_skip_reason = "prototype changed paths outside the contract scope"
    else:
        validation_results = run_validations(
            workspace,
            variant["validation_commands"],
            config.validation_timeout,
            config.validation_pass_env,
        )

    validations_passed = bool(validation_results) and all(
        item["status"] == "passed" for item in validation_results
    )
    post_validation_paths = changed_paths(workspace)
    validation_side_effects = sorted(
        set(post_validation_paths) - set(before_validation)
    )
    mechanical_passed = worker_passed and scope["passed"] and validations_passed
    gate_reasons: list[str] = []
    if not worker_passed:
        gate_reasons.append("worker completion failed")
    if not scope["passed"]:
        gate_reasons.append("scope check failed")
    if validation_skip_reason:
        gate_reasons.append(f"validation skipped: {validation_skip_reason}")
    elif not validations_passed:
        gate_reasons.append("one or more validation commands failed")

    result = {
        "experiment": contract["experiment"],
        "variant": variant_id,
        "workspace": str(workspace),
        "baseline_commit": baseline_commit,
        "isolation": {
            "disposable_git_clone": True,
            "main_worktree_write_isolated": True,
            "os_filesystem_sandbox": False,
            "claude_read_boundary": "Claude permission system; not an OS boundary",
            "write_scope_enforcement": "post-run detection inside disposable clone",
            "validation_commands_os_sandboxed": False,
        },
        "worker": {
            "returncode": returncode,
            "timed_out": timed_out,
            "elapsed_seconds": elapsed,
            "parse_error": parse_error,
            "structured_output": structured,
            "envelope_metadata": {
                key: value
                for key, value in (envelope or {}).items()
                if key not in {"result", "structured_output"}
            },
        },
        "prototype_changes": before_validation,
        "scope": scope,
        "validations": validation_results,
        "validation_skip_reason": validation_skip_reason,
        "validation_side_effects": validation_side_effects,
        "mechanical_gate": {
            "passed": mechanical_passed,
            "reasons": gate_reasons,
        },
        "purpose_gate": {
            "verdict": "pending_main_review",
            "rationale": "",
            "evidence": [],
            "missing_evidence": [],
        },
    }
    (variant_dir / "result.json").write_text(
        json.dumps(result, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
    )
    return result


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description="Run Claude prototype variants in isolated local clones"
    )
    parser.add_argument(
        "--repo", required=True, help="Git repository to prototype against"
    )
    parser.add_argument("--contract", required=True, help="Experiment contract JSON")
    parser.add_argument(
        "--output-dir", help="Artifact directory; defaults to a new /tmp directory"
    )
    parser.add_argument(
        "--variant",
        action="append",
        default=[],
        help="Variant id to run; repeat to select multiple",
    )
    parser.add_argument(
        "--jobs", type=int, default=1, help="Concurrent variant workers (default: 1)"
    )
    parser.add_argument("--claude-bin", default="claude")
    parser.add_argument("--model", default=os.environ.get("CLAUDE_PROTOTYPE_MODEL"))
    parser.add_argument("--effort", choices=["low", "medium", "high"], default="low")
    parser.add_argument("--max-turns", type=int, default=12)
    parser.add_argument(
        "--max-budget-usd", type=float, default=1.0, help="Per-variant budget"
    )
    parser.add_argument(
        "--timeout", type=int, default=600, help="Claude timeout seconds per variant"
    )
    parser.add_argument(
        "--validation-timeout",
        type=int,
        default=300,
        help="Timeout per validation command",
    )
    parser.add_argument(
        "--include-working-tree",
        action="store_true",
        help="Apply tracked staged/unstaged diff to sandboxes",
    )
    parser.add_argument(
        "--include-untracked",
        action="append",
        default=[],
        help="Explicit repo-relative untracked path to copy",
    )
    parser.add_argument(
        "--allowed-tool",
        action="append",
        default=[],
        help="Additional narrowly scoped Claude tool",
    )
    parser.add_argument(
        "--validation-pass-env",
        action="append",
        default=[],
        help="Explicit environment variable to expose to validation commands",
    )
    parser.add_argument(
        "--prepare-only",
        action="store_true",
        help="Prepare sandboxes and prompts without invoking Claude",
    )
    return parser


def main() -> int:
    args = build_parser().parse_args()
    try:
        repo = Path(args.repo).expanduser().resolve()
        contract_path = Path(args.contract).expanduser().resolve()
        if not repo.is_dir():
            raise PrototypeError(f"Repository does not exist: {repo}")
        if (
            run_command(
                ["git", "rev-parse", "--is-inside-work-tree"], cwd=repo
            ).stdout.strip()
            != "true"
        ):
            raise PrototypeError(f"Repository is not a git work tree: {repo}")
        contract = validate_contract(
            json.loads(contract_path.read_text(encoding="utf-8"))
        )
        selected_ids = set(args.variant)
        known_ids = {variant["id"] for variant in contract["variants"]}
        unknown = selected_ids - known_ids
        if unknown:
            raise PrototypeError(f"Unknown variant ids: {', '.join(sorted(unknown))}")
        variants = [
            variant
            for variant in contract["variants"]
            if not selected_ids or variant["id"] in selected_ids
        ]
        if args.jobs < 1 or args.jobs > len(variants):
            raise PrototypeError(f"--jobs must be between 1 and {len(variants)}")
        if (
            args.max_turns < 1
            or args.timeout < 1
            or args.validation_timeout < 1
            or args.max_budget_usd <= 0
        ):
            raise PrototypeError("Turn, timeout, and budget limits must be positive")
        if not args.prepare_only and shutil.which(args.claude_bin) is None:
            raise PrototypeError(f"Claude executable not found: {args.claude_bin}")

        output_dir = (
            Path(args.output_dir).expanduser().resolve()
            if args.output_dir
            else Path(tempfile.mkdtemp(prefix="claude-prototype-validator-"))
        )
        output_dir.mkdir(parents=True, exist_ok=True)
        (output_dir / "contract.json").write_text(
            json.dumps(contract, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )

        tools = list(DEFAULT_TOOLS)
        allowed_tools: list[str] = []
        for tool in args.allowed_tool:
            if tool not in allowed_tools:
                allowed_tools.append(tool)
            exposed_tool = tool.split("(", 1)[0]
            if exposed_tool and exposed_tool not in tools:
                tools.append(exposed_tool)

        config = RunConfig(
            repo=repo,
            output_dir=output_dir,
            claude_bin=args.claude_bin,
            model=args.model,
            effort=args.effort,
            max_turns=args.max_turns,
            max_budget_usd=args.max_budget_usd,
            timeout=args.timeout,
            validation_timeout=args.validation_timeout,
            include_working_tree=args.include_working_tree,
            include_untracked=tuple(args.include_untracked),
            tools=tuple(tools),
            allowed_tools=tuple(allowed_tools),
            validation_pass_env=tuple(args.validation_pass_env),
            prepare_only=args.prepare_only,
        )

        results: list[dict[str, Any]] = []
        with concurrent.futures.ThreadPoolExecutor(max_workers=args.jobs) as executor:
            futures = {
                executor.submit(run_variant, contract, variant, config): variant["id"]
                for variant in variants
            }
            for future in concurrent.futures.as_completed(futures):
                variant_id = futures[future]
                try:
                    results.append(future.result())
                except (
                    Exception
                ) as exc:  # preserve other variant artifacts before reporting
                    results.append(
                        {
                            "variant": variant_id,
                            "state": "runner_failed",
                            "error": str(exc),
                            "mechanical_gate": {
                                "passed": False,
                                "reasons": ["runner failed"],
                            },
                            "purpose_gate": {
                                "verdict": "pending_main_review",
                                "rationale": "",
                                "evidence": [],
                                "missing_evidence": [],
                            },
                        }
                    )
        results.sort(key=lambda item: item["variant"])
        reported_costs = [
            item.get("worker", {}).get("envelope_metadata", {}).get("total_cost_usd")
            for item in results
        ]
        numeric_costs = [
            float(cost) for cost in reported_costs if isinstance(cost, (int, float))
        ]
        summary = {
            "experiment": contract["experiment"],
            "output_dir": str(output_dir),
            "prepare_only": args.prepare_only,
            "budget": {
                "enforced_by": "Claude CLI --max-budget-usd per variant",
                "per_variant_cap_usd": args.max_budget_usd,
                "requested_total_cap_usd": round(
                    args.max_budget_usd * len(variants), 6
                ),
                "reported_total_cost_usd": (
                    round(sum(numeric_costs), 6) if numeric_costs else None
                ),
            },
            "results": results,
        }
        (output_dir / "summary.json").write_text(
            json.dumps(summary, ensure_ascii=False, indent=2) + "\n", encoding="utf-8"
        )
        print(json.dumps(summary, ensure_ascii=False, indent=2))
        if args.prepare_only:
            return 0
        return (
            0
            if all(item.get("mechanical_gate", {}).get("passed") for item in results)
            else 3
        )
    except (OSError, json.JSONDecodeError, PrototypeError) as exc:
        print(f"error: {exc}", file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
