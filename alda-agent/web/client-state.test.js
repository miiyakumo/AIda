import test from "node:test";
import assert from "node:assert/strict";
import {readFileSync} from "node:fs";
import {
  applyCommandResult,
  applyEventPage,
  beginRecovery,
  commandEnvelope,
  commands,
  createClientState,
  finishRecovery,
  markDisconnected,
  recoveryOptionsForServerMessage
} from "./client-state.js";

test("production browser sources contain only the v2 transport", () => {
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const state = readFileSync(new URL("./client-state.js", import.meta.url), "utf8");
  assert.match(app, /alda-agent\.v2/);
  assert.match(app, /\/v2\//);
  assert.doesNotMatch(`${app}\n${state}`, /alda-agent\.v1|\/v1\//);
});

test("structured commands map to protocol DTOs and update IDs/status", () => {
  const state = createClientState();
  assert.deepEqual(commands.projectCreate("Etude"), {
    type: "project_create", params: {name: "Etude"}
  });
  assert.equal(commandEnvelope(state, commands.sessionStart("project-1")).value
    .command.params.project_id, "project-1");
  assert.equal(commandEnvelope(state, commands.sessionSnapshot("session-1")).value
    .protocol_version, 2);
  applyCommandResult(state, {
    type: "session_started",
    value: {
      session_id: "session-1", project_id: "project-1", stream_epoch: 1,
      covered_through_sequence: 1, turns: []
    }
  });
  applyCommandResult(state, {
    type: "turn_started", value: {turn_id: "turn-1", status: "running"}
  });
  assert.deepEqual(
    [state.projectId, state.sessionId, state.turnId, state.turnStatus],
    ["project-1", "session-1", "turn-1", "running"]
  );
});

test("complete event frames advance the processed cursor once", () => {
  const state = createClientState();
  applyEventPage(state, {
    epoch: 1,
    events: [
      {sequence: 2, type: "turn_started", value: {turn_id: "turn-1"}},
      {sequence: 3, type: "question_requested", value: {}}
    ]
  });
  assert.equal(state.lastProcessedSequence, 3);
  applyEventPage(state, {
    epoch: 1,
    events: [{sequence: 3, type: "question_requested", value: {}}]
  });
  assert.equal(state.lastProcessedSequence, 3);
});

test("disconnect reconnect snapshots then resumes retained cursor", () => {
  const state = createClientState();
  state.sessionId = "session-1";
  state.lastProcessedSequence = 7;
  markDisconnected(state);
  assert.equal(state.recoveryCursor, 7);
  assert.deepEqual(beginRecovery(state), commands.sessionSnapshot("session-1"));
  const subscribe = finishRecovery(state, {
    session_id: "session-1", stream_epoch: 2, covered_through_sequence: 9
  });
  assert.equal(subscribe.value.after_sequence, 7);
  assert.equal(subscribe.value.epoch, 2);
});

test("Lagged never replaces the client cursor and replay has no gap or duplicate", () => {
  const lagged = createClientState();
  lagged.sessionId = "session-1";
  lagged.lastProcessedSequence = 2;
  const serverDeliveredSequence = 4;
  assert.equal(serverDeliveredSequence, 4);
  beginRecovery(lagged, recoveryOptionsForServerMessage({
    type: "lagged", value: {last_delivered_sequence: serverDeliveredSequence}
  }));
  const subscribe = finishRecovery(lagged, {
    session_id: "session-1", stream_epoch: 1, covered_through_sequence: 6
  });
  assert.equal(subscribe.value.after_sequence, 2);
  applyEventPage(lagged, {
    epoch: 1,
    events: [3, 4, 5, 6].map(sequence => ({
      sequence, type: "question_requested", value: {}
    }))
  });
  assert.equal(lagged.lastProcessedSequence, 6);
  applyEventPage(lagged, {
    epoch: 1,
    events: [{sequence: 6, type: "question_requested", value: {}}]
  });
  assert.equal(lagged.lastProcessedSequence, 6);
});

test("typed cursor recovery resets to snapshot coverage instead of looping future cursor", () => {
  const state = createClientState();
  state.sessionId = "session-1";
  state.lastProcessedSequence = 99;
  const typedRecovery = recoveryOptionsForServerMessage({
    type: "protocol_error",
    value: {recovery: {type: "fetch_session_snapshot", session_id: "session-1"}}
  });
  assert.deepEqual(typedRecovery, {resetToSnapshot: true});
  beginRecovery(state, typedRecovery);
  assert.equal(finishRecovery(state, {
    session_id: "session-1", stream_epoch: 2, covered_through_sequence: 8
  }).value.after_sequence, 8);
  assert.equal(state.lastProcessedSequence, 8);

  beginRecovery(state, typedRecovery);
  assert.equal(finishRecovery(state, {
    session_id: "session-1", stream_epoch: 3, covered_through_sequence: 10
  }).value.after_sequence, 10);
});
