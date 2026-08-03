---
name: simple-workflow
description: Coordinate a complex task through the smallest useful Brain–Worker–Reviewer workflow, including collaborative requirement clarification and staged review. Use when the user asks to split work into thinking, execution, and independent review units; delegate a task across agents; run work in parallel; or keep a long multi-agent task moving without filling the main agent's context.
---

# Simple Agent Workflow

Treat the main agent as the Master. The Master gathers context, works with the user, delegates, and makes final judgments; do not create a separate orchestrator agent.

## Workflow

1. Decide whether decomposition helps. If one agent can handle the task well, do not split it. Before delegating a complex task, inspect the project deeply enough to give the Brain and Workers grounded context.
2. Ask one Brain agent to examine the goal. Keep the same Brain while it challenges assumptions, identifies material ambiguity, and proposes clarifications. Relay its concise objections or questions to the user and send the user's answers back until the goal is mutually understood.
3. Have the Brain produce the fewest useful tasks. Keep each task to:
   - **Task:** what to do
   - **Done:** how to know it is complete
   - **Depends on:** include only when needed
4. Have Worker agents execute the tasks. Run tasks in parallel only when they are genuinely independent. Group work into phases only when dependencies make phases useful.
5. Review after a meaningful phase or after all work, not after every Worker by default. Review earlier only when later work depends on the result or the risk justifies it.
6. If review finds blocking defects, send the concrete findings to the relevant Worker, then review the corrections. Allow at most two correction rounds per review point; after that, decide as Master or report the unresolved disagreement.
7. Deliver the result with completed work, verification, and remaining blockers.

Use the fewest agents, tasks, and coordination steps necessary.

## Worker Instructions

Write model-specific Worker prompts as Master; do not ask the user to prepare them.

- Give a capable Worker a concise goal, constraints, and done condition. Let it investigate and choose the implementation.
- Give a weaker Worker a smaller task plus relevant context, explicit steps, boundaries, verification commands, and a short return format.
- When capability is unknown, use medium detail and add guidance only if the first result shows it is needed.

Do not hardcode model names. Increase detail with task ambiguity and risk; decrease it with Worker capability.

Ask each Worker to return only what changed, where the result is, how it was verified, and any blocker. Keep detailed work in task artifacts rather than copying execution transcripts into the main context.

## Review

Review for completion, not perfection.

- Let the Master choose review boundaries. Batch related Worker results and judge the phase outcome together when practical.
- Judge only against the user's request, the done conditions, and concrete regressions.
- Reject only for an unmet condition or a reproducible correctness, security, or regression defect.
- Treat style preferences and optional improvements as non-blocking suggestions.
- After the first review, check the requested corrections and regressions. Do not keep expanding scope. Add a new blocker only for a serious earlier omission or a defect introduced by the correction.

## Long Runs

Run Workers in separate background contexts and rely on completion notifications. Do not repeatedly poll or copy full logs into the main context.

For work likely to outlive the current context, use the host's persistent goal or task state when available. Otherwise keep one compact, overwritten checkpoint containing only the goal, current phase, each task's done condition, status, agent identifier, attempt count, artifact locations, key decisions, and next action. Update it before background dispatch and whenever task state changes.

If a Worker fails or disappears, treat its outcome as unknown. Check whether the agent is still running, then inspect task artifacts before retrying; never assume failure means no files changed. Preserve valid partial work and retry once with a fresh Worker that receives the original task, done condition, existing artifacts, and failure context. After another failure, inspect and replan as Master or ask the user.