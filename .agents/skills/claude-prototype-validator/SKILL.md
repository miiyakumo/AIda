---
name: claude-prototype-validator
description: Validate uncertain software designs through isolated Claude Code prototype experiments. Use when Codex has proposed a design but cannot cheaply establish feasibility, needs to compare two or more designs under equivalent conditions, needs empirical evidence before production implementation, or the user asks for 快速验证、方案对比、可行性原型、Claude 原型/子任务. Codex writes the detailed experiment contract, Claude implements disposable prototypes, and Codex owns purpose-gate verification and synthesis. Do not use for routine implementation or when static reasoning and existing tests already answer the question.
---

# Claude Prototype Validator

Use Claude as an isolated prototype worker. Keep planning, experiment validity, evidence review, and the final conclusion with the main Codex agent.

## Preserve the role boundary

- Main Codex: investigate the repository, state the uncertainty, design the experiment, write the implementation plan, inspect actual artifacts, run the purpose gate, and synthesize results.
- Claude worker: implement one bounded disposable prototype inside its assigned sandbox and report what it did.
- Prototype: evidence for a decision, not production code. Never merge or copy it into the product without a separate implementation decision and normal review.

Treat every worker completion claim as untrusted. A valid process exit or passing test is not proof that the experiment answered the main agent's question.

## Follow the workflow

### 1. Frame one decision

Write a falsifiable question before delegating. Examples:

- Can this parser design preserve streaming behavior without buffering the full input?
- Which of designs A and B produces a simpler failure model under the same acceptance cases?
- Can the proposed API be implemented without changing the public compatibility boundary?

Record why static inspection is insufficient and what observation would change the design decision. If a cheap deterministic check already answers the question, run it directly and skip delegation.

### 2. Investigate before designing the prototype

Read the relevant code, repository instructions, existing tests, and current behavior. Identify the smallest slice that preserves the uncertainty being tested. Exclude production polish, broad refactors, migrations, and unrelated cleanup.

For comparisons, keep the baseline, fixtures, success criteria, validation commands, and measurement method identical across variants. Vary only the design choice under examination.

### 3. Write the experiment contract

Read [references/experiment-contract.md](references/experiment-contract.md) and create a complete JSON contract. Each variant must contain a detailed implementation plan rather than a request for Claude to design the solution.

Require at least:

- the decision question and validation purpose;
- relevant baseline facts and shared constraints;
- evidence required to answer the question;
- one or more variants with explicit design and ordered implementation steps;
- allowed and forbidden paths;
- observable success criteria and deterministic validation commands;
- non-goals that keep the prototype disposable.

Do not hide the main agent's preferred conclusion in the prompt. For a comparison, make each variant independently implementable and label variants neutrally.

### 4. Run isolated prototype workers

Run from the target repository:

```bash
python .agents/skills/claude-prototype-validator/scripts/run_prototypes.py \
  --repo . \
  --contract /path/to/experiment.json \
  --model "$CLAUDE_PROTOTYPE_MODEL"
```

Omit `--model` when no worker model is configured. The runner creates one local clone per variant under a temporary output directory, invokes a fresh non-persistent Claude session, captures structured output, records the real diff, checks path scope, and runs only the validation commands written by the main agent.

This provides write isolation from the main worktree, not an OS security sandbox. Claude reads are governed by Claude's permission system, and changed-path scope is detected after the worker exits. Validation commands also run without an OS filesystem or network sandbox. Use only trusted repositories and contracts; do not expose secrets or production credentials.

Useful controls:

- `--variant ID`: run selected variants only; repeat as needed.
- `--jobs N`: run independent comparison variants concurrently.
- `--max-budget-usd N`: set the per-variant API budget; default is `1.00`.
- `--max-turns N`, `--timeout N`, `--effort low|medium|high`: bound worker effort.
- `--allowed-tool 'Bash(command-pattern *)'`: permit a narrowly scoped Claude Bash tool. Bash is unavailable to Claude by default; the runner itself still executes the contract's validation commands afterward.
- `--validation-pass-env NAME`: expose one additional environment variable to validation commands. They otherwise receive a reduced environment and an isolated temporary `HOME`.
- `--include-working-tree`: apply tracked staged and unstaged changes to every sandbox.
- `--include-untracked PATH`: explicitly copy a required untracked path. Never include secrets or unrelated user files.
- `--prepare-only`: validate the contract and prepare sandboxes/prompts without spending model tokens.

Keep default safe mode and scoped tools. Do not add `bypassPermissions` or broad `Bash` merely to avoid a worker failure.

Budget enforcement is delegated to Claude CLI and may show a small accounting overage in its final report. Leave margin below the user's true ceiling and use `summary.json`'s reported total for cost accounting. Treat `--max-turns` as a worker-loop control, not an independent billing guarantee.

### 5. Inspect mechanical evidence

For every variant, read `result.json`, `diff.patch`, `changed-files.json`, validation output, and the actual sandbox files. Check:

1. Claude returned structured output and did not report blocked or failed.
2. Actual changed paths stay inside `allowed_scope` and outside `forbidden_scope`.
3. The diff implements the contract's ordered plan rather than a substitute design.
4. Validation commands ran against the prototype and passed.
5. Evidence required by the contract actually exists.

The runner's `mechanical_gate.passed` is only a prerequisite for semantic review. If it fails, do not claim the design failed: distinguish a broken experiment, worker failure, environmental failure, and genuine design infeasibility.

### 6. Run the purpose gate first

Before comparing performance, elegance, or maintainability, answer:

> Did this prototype preserve the uncertainty under investigation and produce evidence capable of answering the main agent's original question?

Record exactly one verdict per variant with the bundled recorder:

```bash
python .agents/skills/claude-prototype-validator/scripts/record_assessment.py \
  --result /tmp/run/variants/VARIANT/result.json \
  --verdict yes \
  --rationale "The prototype preserves the design choice and exercises the decisive cases." \
  --evidence "validation command and diff location"
```

The recorder rejects `yes` when the mechanical gate failed or when no evidence is supplied. Use one of these verdicts:

- `yes`: the prototype faithfully represents the proposed design and the evidence answers the question;
- `no`: the prototype implemented the wrong thing, omitted a decisive condition, or otherwise cannot answer the question;
- `indeterminate`: the experiment is faithful but the evidence is insufficient or conflicting.

If the verdict is `no`, do not analyze the prototype as evidence about the design. Issue at most one focused repair contract when the defect is clearly in prototype execution rather than the design. Otherwise report an invalid experiment and the missing evidence.

If the verdict is `indeterminate`, state what additional observation is needed. Run another prototype only when its expected decision value justifies the cost.

### 7. Analyze only purpose-valid results

For a single design, summarize:

- what the evidence establishes and does not establish;
- feasibility, constraints discovered, implementation complexity, and failure modes;
- differences between the proposed design and the prototype;
- whether to adopt, revise, reject, or investigate further.

For comparisons, use the same dimensions for every purpose-valid variant. Separate measured observations from qualitative judgment. Do not choose a winner when variants were tested under materially different conditions or when the decisive dimension was not measured.

### 8. Deliver the decision record

Lead with the design decision, then report:

1. original question and why a prototype was needed;
2. purpose-gate verdict for each variant;
3. evidence table with commands, observations, and artifact paths;
4. analysis and recommendation;
5. limitations, invalid variants, and remaining uncertainty;
6. explicit note that prototype branches/sandboxes were not production changes.

Never report successful validation when the recorded purpose gate is not `yes`.

## Stop conditions

- Stop when the main question is answered with sufficient evidence.
- Stop after the same prototype defect recurs twice; revise the experiment instead of retrying.
- Stop when validation requires production credentials, irreversible external effects, or access outside the authorized repository scope.
- Ask the user before materially expanding the experiment, budget, or decision scope.
