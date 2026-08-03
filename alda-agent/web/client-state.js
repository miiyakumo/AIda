"use strict";

export function createClientState() {
  return {
    commandCounter: 0,
    projectId: "",
    sessionId: "",
    turnId: "",
    turnStatus: "",
    epoch: 1,
    lastProcessedSequence: 0,
    connected: false,
    recovering: false,
    recoveryCursor: null,
    recoveryMode: null
  };
}

export function commandEnvelope(state, command, purpose = "command") {
  state.commandCounter += 1;
  return {
    type: "command",
    value: {
      protocol_version: 2,
      client_id: "pwa",
      client_command_id: `pwa-${purpose}-${state.commandCounter}`,
      command
    }
  };
}

export const commands = {
  projectCreate: name => ({type: "project_create", params: {name}}),
  sessionStart: projectId => ({
    type: "session_start",
    params: {project_id: projectId}
  }),
  sessionSnapshot: sessionId => ({
    type: "session_snapshot",
    params: {session_id: sessionId}
  }),
  turnStart: (sessionId, prompt) => ({
    type: "turn_start",
    params: {session_id: sessionId, prompt}
  }),
  turnCancel: (sessionId, turnId) => ({
    type: "turn_cancel",
    params: {session_id: sessionId, turn_id: turnId}
  })
};

export function applyCommandResult(state, result) {
  const value = result.value;
  if (result.type === "project_created" || result.type === "project_snapshot") {
    state.projectId = value.project_id;
  }
  if (result.type === "session_started" || result.type === "session_snapshot") {
    state.sessionId = value.session_id;
    state.projectId = value.project_id;
    state.epoch = value.stream_epoch;
    const latest = value.turns.at(-1);
    if (latest) {
      state.turnId = latest.turn_id;
      state.turnStatus = latest.status;
    }
  }
  if (result.type === "turn_started" || result.type === "turn_cancelled") {
    state.turnId = value.turn_id;
    state.turnStatus = value.status;
  }
  if (result.type === "turn_already_terminal") {
    state.turnId = value.turn_id;
    state.turnStatus = value.terminal_status;
  }
}

export function applyEventPage(state, page) {
  for (const event of page.events) {
    if (event.sequence <= state.lastProcessedSequence) continue;
    if (event.type === "turn_started") state.turnId = event.value.turn_id;
    if (event.type === "turn_completed") {
      state.turnId = event.value.turn_id;
      state.turnStatus = event.value.status;
    }
    state.lastProcessedSequence = event.sequence;
  }
  state.epoch = page.epoch;
}

export function markDisconnected(state) {
  state.connected = false;
  state.recovering = Boolean(state.sessionId);
  state.recoveryCursor = state.sessionId ? state.lastProcessedSequence : null;
  state.recoveryMode = state.sessionId ? "preserve_client_cursor" : null;
}

export function beginRecovery(state, {resetToSnapshot = false} = {}) {
  state.recovering = true;
  state.recoveryMode = resetToSnapshot ?
    "reset_to_snapshot" : "preserve_client_cursor";
  state.recoveryCursor = resetToSnapshot ? null : state.lastProcessedSequence;
  return commands.sessionSnapshot(state.sessionId);
}

export function recoveryOptionsForServerMessage(message) {
  if (message.type === "lagged") return {resetToSnapshot: false};
  if (message.type === "protocol_error" &&
      message.value.recovery?.type === "fetch_session_snapshot") {
    return {resetToSnapshot: true};
  }
  return null;
}

export function finishRecovery(state, snapshot) {
  const afterSequence = state.recoveryMode === "preserve_client_cursor" ?
    state.recoveryCursor : snapshot.covered_through_sequence;
  state.sessionId = snapshot.session_id;
  state.epoch = snapshot.stream_epoch;
  state.lastProcessedSequence = afterSequence;
  state.recovering = false;
  state.recoveryCursor = null;
  state.recoveryMode = null;
  return {
    type: "subscribe",
    value: {
      session_id: snapshot.session_id,
      epoch: snapshot.stream_epoch,
      after_sequence: afterSequence
    }
  };
}
