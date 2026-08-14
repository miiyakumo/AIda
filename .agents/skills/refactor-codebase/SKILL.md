---
name: refactor-codebase
description: Audit and execute evidence-based, system-wide codebase refactors through independent subagent review, implementation, and verification. Use for project-wide architecture simplification, comprehensive refactoring, code-size reduction, duplicate abstraction, dead or obsolete implementation removal, or strict review-implement-re-review requests with reviewer adjudication; triggers include 简化代码架构、全面重构、缩减代码规模、抽象重复实现、移除无用实现、严格评审、独立复核、subagent 审议. Preserve observable behavior by default while allowing justified local complexity and internal breaking changes that reduce total system complexity. Do not use for routine single-file cleanup, ordinary code review, feature implementation, or speculative redesign.
---

# Refactor Codebase

Run a bounded Audit–Implement–Verify workflow. Treat code and executable behavior as evidence; use documentation for discovery, then correct it when the resulting architecture makes it stale.

Keep the main agent as coordinator and final decision-maker. Use distinct subagents for independent roles. If subagents are unavailable, disclose that independent review cannot be provided and ask whether to continue with a single-agent workflow.

## Architectural principle

Optimize for the simplest coherent system, not the smallest local implementation.

Evaluate the whole system through module responsibilities, dependency direction, state ownership, invariants, duplicated knowledge, common change paths, and failure boundaries. Treat code size, file count, and abstraction count as supporting signals rather than goals.

Allow additional local structure, types, indirection, or code only when current code demonstrates that it removes greater system-wide complexity or contains essential complexity behind a clear boundary. Require each such proposal to identify:

- the current system-level problem and code evidence;
- the affected modules and dependency paths;
- the complexity removed, unified, or contained;
- why a simpler alternative is insufficient;
- the cost introduced and how to verify the net benefit.

Reject arguments based only on elegance, symmetry, convention, or hypothetical future extensibility. Do not reject a proposal merely because it increases LOC or makes one module more sophisticated.

Preserve user-observable behavior unless the user authorizes a change. Do not preserve internal compatibility when a breaking internal change yields a clearer system. Never trade away correctness, security, or data integrity for architectural neatness.

## Evidence and independence

- Make every reviewer inspect relevant code, tests, and diffs directly before reading another agent's conclusions.
- Give independent agents the goal, scope, constraints, and raw artifacts without leaking the expected answer.
- Require file paths, call paths, test results, or reproducible behavior for blocking claims.
- Separate blocking defects from optional suggestions. Reject only for unmet requirements, concrete regressions, or unsupported architectural claims.
- Seek agreement through evidence, not deference. Allow at most two response rounds at each review point; record remaining disagreement and let the main agent decide.
- Keep audit and review agents read-only. Let only the implementation agent edit production files.
- Avoid temporary process documents in the repository. Persist only architecture or iteration knowledge that the repository's documentation rules require.

## Phase 0: Establish the baseline

Have the main agent inspect repository instructions, relevant documentation, code layout, tests, and working-tree state before delegation. Determine:

- observable behaviors and public interfaces that must remain stable;
- available test, lint, type-check, and build commands;
- current architecture, dependency paths, and known documentation conflicts;
- unrelated user changes that must be preserved.

Run the cheapest representative baseline checks. Report pre-existing failures and do not attribute them to the refactor.

## Phase 1: Audit and adjudicate

Start one Audit agent and one Audit Critic agent in separate contexts.

Ask the Audit agent to inspect the system as a whole before proposing changes. Require each finding to contain:

```text
ID:
Evidence:
System problem:
Proposed change:
System-wide benefit:
Local cost or added complexity:
Simpler alternative considered:
Behavior and validation impact:
```

Ask the Audit Critic to inspect the same raw code independently. Only after its independent pass, provide the Audit findings and require a verdict of `accept`, `revise`, or `reject` for each item with code evidence.

Let the pair exchange evidence for at most two rounds. Have the main agent resolve remaining disagreements, discard speculative or low-value work, and freeze the accepted scope. Do not let later phases silently expand it.

## Phase 2: Implement the frozen scope

Start a fresh Implementation agent. Provide the frozen findings, constraints, baseline, and required verification—not the reviewers' full discussion.

Require the agent to re-check each finding against current code and report stale or incorrect assumptions instead of forcing the planned change. Implement accepted work in the fewest coherent batches, validating after each batch when practical.

Permit broad internal restructuring when it is necessary for the accepted system-level outcome. Do not add compatibility layers, speculative extension points, or unrelated cleanup unless the frozen scope requires them.

Require the agent to return only:

- changed files and architectural outcome;
- accepted findings completed or declined with evidence;
- verification commands and results;
- remaining risks or blockers.

## Phase 3: Verify and adjudicate

Start a fresh Verification agent and a fresh Verification Critic agent. Keep both independent from the implementation discussion.

Ask the Verification agent to inspect the actual diff, surrounding code, baseline, tests, and frozen scope. Check:

- preserved observable behavior and absence of regressions;
- completion of every accepted finding;
- dependency direction, ownership, invariants, and total system complexity;
- unjustified indirection, compatibility residue, dead paths, or scope expansion;
- documentation that became inaccurate because of the refactor.

Ask the Verification Critic to inspect the same artifacts independently before judging the Verification findings. Require it to challenge both false positives and missed blockers with reproducible evidence.

Let the pair exchange evidence for at most two rounds. Send confirmed blockers to the Implementation agent, then have the verification pair check only the corrections and regressions. Do not expand scope during correction.

Have the main agent adjudicate unresolved disagreements. Do not call the work complete because agents reached consensus; call it complete only when the frozen scope and verification conditions are satisfied.

## Completion

Run the relevant final checks and inspect the final diff. Update durable architecture or iteration documentation only when the repository rules and the final change warrant it.

Report:

- the system-level simplification achieved;
- justified local complexity introduced;
- behavior and verification results;
- code-size changes as context, not as proof of quality;
- rejected proposals and unresolved disagreements;
- remaining blockers or risks.
