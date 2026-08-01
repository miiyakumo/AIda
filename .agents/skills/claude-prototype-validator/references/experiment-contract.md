# Experiment Contract

Use this contract to turn design uncertainty into a bounded prototype experiment. The main Codex agent authors it; the Claude worker must not redesign it.

## Contract shape

Start from [example-contract.json](example-contract.json). Replace every example fact, path, criterion, and command with evidence grounded in the target repository.

## Field rules

- `version`: use `1`.
- `experiment`: use a short filesystem-safe identifier.
- `question`: ask one falsifiable design question.
- `purpose`: state which main-agent decision the evidence will support.
- `why_prototype`: explain why inspection or a smaller deterministic check is insufficient.
- `baseline`: cite concrete repository facts, files, behaviors, or measurements.
- `shared_constraints`: apply the same constraints to every variant.
- `evidence_required`: list observable evidence needed for the purpose gate.
- `non_goals`: remove production polish and unrelated improvements.
- `variants`: provide one variant for feasibility validation and two or more for comparison.

Each variant requires:

- a neutral `id` containing letters, numbers, dots, underscores, or dashes;
- the exact `design` being tested;
- an ordered, sufficiently detailed `implementation_plan`;
- non-empty `allowed_scope` and explicit `forbidden_scope`;
- observable `success_criteria`;
- deterministic `validation_commands` authored by the main agent.

Validation commands run through `bash -lc` inside the disposable clone with a reduced environment and temporary `HOME`, but without an OS filesystem or network sandbox. Use only trusted, repository-local commands without production credentials, network mutations, deployment, or irreversible external effects. Pass a required toolchain variable explicitly with `--validation-pass-env NAME`.

## Scope syntax

- `path/to/file` matches one file.
- `path/to/directory/` matches everything below that directory.
- Shell-style patterns such as `prototypes/**/*.rs` are allowed.
- A path that matches `forbidden_scope` is always a violation, even if it also matches `allowed_scope`.

Keep prototype paths separate from production paths whenever possible. If modifying production files is essential to test the design, use the smallest explicit file list and remember that the runner operates only inside its disposable clone.

## Comparison fairness

For design comparisons:

1. Use the same repository baseline.
2. Use equivalent implementation depth.
3. Use identical fixtures and success cases.
4. Use the same measurement method and resource limits.
5. Avoid giving one variant implementation hints that the others do not receive.
6. Record any unavoidable asymmetry before interpreting results.

Do not combine separate decision questions into one experiment. Create a second contract when resolving one uncertainty changes the assumptions of the next.
