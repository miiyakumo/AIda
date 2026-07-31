# Alda Agent implementation

This directory contains the released A1–A4 development protocol slice against
the approved [MVP design](../docs/design/mvp-design.md). It is a development
foundation for the remaining MVP slices, not the finished MVP.

Implemented now:

- versioned typed commands for projects, Session snapshots, Fake Turn
  start/cancel, and paged Session event resume;
- formal Turn states and structured lifecycle events with monotonic sequence
  numbers, a fixed in-memory epoch, and machine-readable cursor recovery;
- structured `PendingQuestion` and `PendingApproval` projections, full
  requested/resolved/owner-abort facts, and replay through the same reducer
  used by online command handling;
- a bounded `bars_8`/`bars_16` creative choice followed by a separate Fake
  Model Egress approval bound to a versioned SHA-256 subject digest;
- an in-memory content-addressed Fake Alda fixture store: immutable blobs
  deduplicate by hash while occurrence manifests preserve each Turn's actual
  Project/Session/Turn provenance;
- occurrence-based `artifact.manifest` queries and authenticated, hash-only
  HTTP downloads resolved through the same bounded App Service actor;
- a same-origin static PWA shell, one-time five-minute bootstrap exchange,
  HttpOnly browser cookie, and `alda-agent.v1` WebSocket event recovery;
- independently bounded command/query actor queues with 8:1 weighted
  scheduling, bounded WebSocket connections, polling, frames, and outbound
  buffering;
- a bounded Tokio channel feeding one in-memory App Service writer;
- an in-memory B1 domain projection with explicit Score/Take/Branch identity,
  immutable Revision DAG rules, lifecycle/constraint gates, branch-head CAS,
  deterministic replay digests, and versioned Project/Revision read DTOs;
- a Linux-only descriptor-relative B2 Artifact Store with a durable instance
  manifest, streaming SHA-256 verification, atomic no-replace deduplication,
  same-handle reads, durable pins, and opaque commit receipts; it is not yet
  wired into the development App Service;
- a Linux-only B3a Project transaction-log foundation with checksummed batches,
  byte-exact durable command replies, trusted stored-event conversion,
  single-writer recovery/repair typestates, and validated checkpoints; it is
  not yet wired into the development App Service;
- a separate B3b Session Rollout foundation with authoritative prompt and
  approval-subject replay, command-only durability, restart reconciliation,
  cursor-stable recovery, and descriptor-relative catalog validation; it is
  not yet wired into the development App Service;
- per-client command idempotency, conflicting-payload rejection, and distinct
  new-command handling when an already-terminal Turn is cancelled again;
- a loopback-only HTTP adapter requiring exact Host and Origin plus a valid
  bearer token; the CLI derives Origin from its validated loopback endpoint;
- a thin CLI that calls the same HTTP command contract;
- unit and real loopback HTTP tests.

Not implemented yet:

- production Project/Session persistence integration, retention/compaction,
  and end-to-end process restart recovery;
- a process instance lock or local IPC transport;
- Agent Runtime, real providers, Alda tools, revisions, audition, MIDI, or
  audio artifacts;
- real Provider calls, permission policy, sealed action plans, or approved
  side-effect execution.

The server therefore labels itself as a development Local Service. It must not
be treated as the finished MVP or exposed outside loopback.

## Run

Set a development-only session token in the environment, then start the server:

```bash
export ALDA_AGENT_SESSION_TOKEN='replace-with-a-local-development-token'
cargo run -- serve
```

The service prints a one-time browser bootstrap code to its trusted terminal.
Open `http://127.0.0.1:37891/`, enter that code within five minutes, then use
the minimal client. The browser receives an `HttpOnly; SameSite=Strict`
process-lifetime cookie. Because this development origin is plain loopback
HTTP, the cookie cannot reliably use `Secure`; a future HTTPS deployment must
add it. Codes and cookie values never belong in URLs or browser storage.

The WebSocket reconnect flow is: retain the last fully processed event
sequence, reconnect using subprotocol `alda-agent.v1`, fetch
`session.snapshot`, then subscribe from the retained cursor. On typed cursor
recovery errors, fetch a fresh snapshot. A disconnect does not cancel a Turn.

In another terminal using the independent CLI bearer environment value:

```bash
cargo run -- project create --command-id create-1 --name 'My project'
cargo run -- project snapshot --command-id snapshot-1 --project-id project-1
cargo run -- session start --command-id session-1 --project-id project-1
cargo run -- session snapshot --command-id snapshot-2 --session-id session-1
cargo run -- turn start --command-id turn-1 --session-id session-1 \
  --prompt 'Write a short etude'
cargo run -- event resume --command-id resume-1 --session-id session-1 \
  --epoch 1 --after-sequence 0
cargo run -- question respond --command-id answer-1 --session-id session-1 \
  --question-id question-1 --choice-id bars_8
cargo run -- approval respond --command-id approve-1 --session-id session-1 \
  --approval-id approval-1 --digest-algorithm sha256 \
  --digest-schema-version 1 --digest-value '<value-from-session-snapshot>' \
  --decision approve
cargo run -- artifact manifest --command-id manifest-1 --project-id project-1 \
  --artifact-occurrence-id artifact-occurrence-1
```

Alternatively, cancel a still-pending Turn before responding:

```bash
cargo run -- turn cancel --command-id cancel-1 --session-id session-1 \
  --turn-id turn-1
```

Successful protocol replies are written to stdout. Transport and protocol
errors are written to stderr and return a non-zero exit status.

The manifest supplies the content hash. An authenticated HTTP download can be
requested without giving the service a filename or filesystem target:

```bash
curl -H "Authorization: Bearer $ALDA_AGENT_SESSION_TOKEN" \
  -H 'Origin: http://127.0.0.1:37891' \
  -H 'X-Alda-Project-Id: project-1' \
  http://127.0.0.1:37891/v1/artifacts/<64-lowercase-sha256-hex>
```

All A1–A4 state, browser sessions, events, command replies, and Artifacts exist only for the lifetime
of one service process. Epoch `1` does not imply durable recovery: restarting
the process loses this slice's projects, sessions, turns, events, and
idempotency records. Persistent restart recovery belongs to a later slice.
Approving the A2 fixture only advances its in-memory Fake Turn state machine;
it never sends model data, writes files, plays audio, or authorizes a real
side effect.
The A3 Alda bytes are a deterministic, size-bounded fixture. They are not
parsed by Alda, are not a Revision, and are not stored durably. Disk staging,
fsync/rename, Project replay, orphan cleanup, and formal Revision/Artifact
references remain later work.
The PWA is a development protocol client, not the slice D score workspace: it
does not provide notation, MIDI, audition, feedback, Takes, or Accept.

## Verify

```bash
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test
```
