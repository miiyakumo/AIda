# Alda Agent implementation

This directory contains the first implementation slice of the approved
[MVP design](../docs/design/mvp-design.md).

Implemented now:

- versioned typed commands for initialize, project creation, and snapshots;
- a bounded Tokio channel feeding one in-memory App Service writer;
- per-client command idempotency and conflicting-payload rejection;
- a loopback-only HTTP adapter with Host, Origin, and bearer-token checks;
- a thin CLI that calls the same HTTP command contract;
- unit and real loopback HTTP tests.

Not implemented yet:

- persistent Project/Session logs and crash recovery;
- a process instance lock or local IPC transport;
- PWA bootstrap, WebSocket events, and stream resume;
- Agent Runtime, providers, Alda tools, revisions, audition, or artifacts.

The server therefore labels itself as a development Local Service. It must not
be treated as the finished MVP or exposed outside loopback.

## Run

Set a development-only session token in the environment, then start the server:

```bash
export ALDA_AGENT_SESSION_TOKEN='replace-with-a-local-development-token'
cargo run -- serve
```

In another terminal using the same environment value:

```bash
cargo run -- project create --command-id create-1 --name 'My project'
cargo run -- project snapshot --command-id snapshot-1 --project-id project-1
```

Successful protocol replies are written to stdout. Transport and protocol
errors are written to stderr and return a non-zero exit status.

## Verify

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
