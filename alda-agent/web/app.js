"use strict";

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

const byId = id => document.getElementById(id);
const statusNode = byId("status");
const outputNode = byId("output");
const identityNode = byId("identity");
const questionNode = byId("questions");
const approvalNode = byId("approvals");
const approveButton = byId("approve");
const denyButton = byId("deny");
const artifactNode = byId("artifact");
const state = createClientState();
let socket;
let pendingApproval;
let reconnectTimer;

function show(value) {
  outputNode.textContent = typeof value === "string" ?
    value : JSON.stringify(value, null, 2);
}

function renderIdentity() {
  identityNode.textContent = [
    `Project: ${state.projectId || "—"}`,
    `Session: ${state.sessionId || "—"}`,
    `Turn: ${state.turnId || "—"}`,
    `Turn status: ${state.turnStatus || "—"}`,
    `Processed sequence: ${state.lastProcessedSequence}`
  ].join("\n");
}

function sendWs(value) {
  if (!socket || socket.readyState !== WebSocket.OPEN) {
    statusNode.textContent = "Event connection is not ready.";
    return false;
  }
  socket.send(JSON.stringify(value));
  return true;
}

function sendCommand(command, purpose = "command") {
  return sendWs(commandEnvelope(state, command, purpose));
}

function renderQuestion(question) {
  questionNode.textContent = "";
  const prompt = document.createElement("p");
  prompt.textContent = question.prompt;
  questionNode.appendChild(prompt);
  question.choices.forEach(choice => {
    const button = document.createElement("button");
    button.textContent = choice.label;
    button.addEventListener("click", () => sendCommand({
      type: "question_respond",
      params: {
        session_id: question.session_id,
        question_id: question.question_id,
        choice_id: choice.choice_id
      }
    }, "question"));
    questionNode.appendChild(button);
  });
}

function renderApproval(approval) {
  pendingApproval = approval;
  approvalNode.textContent = [
    `Action: ${approval.payload.action}`,
    `Effect: ${approval.payload.effect}`,
    `Target: ${approval.payload.target}`,
    `Scope: ${approval.payload.scope}`,
    `Impact: ${approval.payload.estimated_impact}`,
    `Subject: ${approval.approval_subject_digest.algorithm}:` +
      `${approval.approval_subject_digest.schema_version}:` +
      approval.approval_subject_digest.value
  ].join("\n");
  approveButton.disabled = false;
  denyButton.disabled = false;
}

function safeArtifactName(manifest) {
  const occurrence = String(manifest.artifact_occurrence_id)
    .replace(/[^a-zA-Z0-9_-]/g, "_").slice(0, 80);
  return `alda-${occurrence || "artifact"}.alda`;
}

function renderArtifact(manifest) {
  artifactNode.textContent = "";
  const summary = document.createElement("p");
  summary.textContent = `${manifest.artifact_occurrence_id} ${manifest.artifact_hash} ` +
    `${manifest.size_bytes} bytes (${manifest.durability})`;
  const button = document.createElement("button");
  button.textContent = "Download verified Alda fixture";
  button.addEventListener("click", async () => {
    button.disabled = true;
    try {
      const hex = manifest.artifact_hash.replace("sha256:", "");
      const response = await fetch(`/v2/artifacts/${hex}`, {
        credentials: "same-origin",
        headers: {"X-Alda-Project-Id": manifest.project_id}
      });
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const blob = await response.blob();
      const objectUrl = URL.createObjectURL(blob);
      try {
        const link = document.createElement("a");
        link.href = objectUrl;
        link.download = safeArtifactName(manifest);
        document.body.appendChild(link);
        link.click();
        link.remove();
      } finally {
        URL.revokeObjectURL(objectUrl);
      }
      statusNode.textContent = `Downloaded ${blob.size} bytes.`;
    } catch (error) {
      statusNode.textContent = `Download failed: ${error.message}`;
    } finally {
      button.disabled = false;
    }
  });
  artifactNode.append(summary, button);
}

function requestRecovery({resetToSnapshot = false} = {}) {
  if (!state.sessionId) return;
  statusNode.textContent = "Recovering: fetching a Session snapshot…";
  sendCommand(beginRecovery(state, {resetToSnapshot}), "recovery");
}

function handleCommandReply(reply) {
  if (reply.status !== "success") {
    statusNode.textContent = `Command failed: ${reply.error.code}`;
    return;
  }
  const result = reply.result;
  applyCommandResult(state, result);
  renderIdentity();
  if (result.type === "session_started") {
    sendWs(finishRecovery(state, result.value));
    statusNode.textContent = "Session started; event subscription active.";
  } else if (result.type === "session_snapshot" && state.recovering) {
    sendWs(finishRecovery(state, result.value));
    statusNode.textContent = "Recovery snapshot applied; event subscription resumed.";
  } else {
    statusNode.textContent = `Command succeeded: ${result.type}`;
  }
  if (result.type === "approval_decided" && result.value.artifact_manifest) {
    renderArtifact(result.value.artifact_manifest);
  }
}

function handleServer(value) {
  show(value);
  if (value.type === "command_reply") {
    handleCommandReply(value.value);
    return;
  }
  if (value.type === "lagged") {
    statusNode.textContent =
      `Server delivered through ${value.value.last_delivered_sequence}; ` +
      `recovering from client-processed ${state.lastProcessedSequence}.`;
    requestRecovery(recoveryOptionsForServerMessage(value));
    return;
  }
  if (value.type === "protocol_error") {
    if (value.value.recovery?.type === "fetch_session_snapshot") {
      requestRecovery(recoveryOptionsForServerMessage(value));
    } else {
      statusNode.textContent = `Protocol error: ${value.value.code}`;
    }
    return;
  }
  if (value.type !== "session_events") return;
  for (const item of value.value.page.events) {
    if (item.type === "question_requested") renderQuestion(item.value.question);
    if (item.type === "approval_requested") renderApproval(item.value.approval);
  }
  applyEventPage(state, value.value.page);
  renderIdentity();
}

async function bootstrap() {
  const codeNode = byId("bootstrap-code");
  const response = await fetch("/v2/bootstrap", {
    method: "POST",
    credentials: "same-origin",
    headers: {"Content-Type": "application/json"},
    body: JSON.stringify({code: codeNode.value})
  });
  codeNode.value = "";
  statusNode.textContent = response.ok ?
    "Browser session established; connecting events…" : "Bootstrap rejected.";
  if (response.ok) reconnect();
}

function reconnect() {
  clearTimeout(reconnectTimer);
  if (socket) {
    socket.onclose = null;
    socket.close();
  }
  socket = new WebSocket(`ws://${location.host}/v2/ws`, "alda-agent.v2");
  socket.onopen = () => {
    state.connected = true;
    statusNode.textContent = state.sessionId ?
      "Connected; recovering Session events…" : "Connected; ready to create a Project.";
    if (state.sessionId) requestRecovery();
  };
  socket.onmessage = event => handleServer(JSON.parse(event.data));
  socket.onclose = () => {
    markDisconnected(state);
    statusNode.textContent = "Disconnected; reconnect and snapshot recovery scheduled.";
    reconnectTimer = setTimeout(reconnect, 1000);
  };
}

byId("bootstrap").addEventListener("click", bootstrap);
byId("reconnect").addEventListener("click", reconnect);
byId("project-create").addEventListener("click", () =>
  sendCommand(commands.projectCreate(byId("project-name").value), "project"));
byId("session-start").addEventListener("click", () =>
  sendCommand(commands.sessionStart(state.projectId), "session"));
byId("session-snapshot").addEventListener("click", () =>
  sendCommand(commands.sessionSnapshot(state.sessionId), "snapshot"));
byId("turn-start").addEventListener("click", () =>
  sendCommand(commands.turnStart(state.sessionId, byId("turn-prompt").value), "turn"));
byId("turn-cancel").addEventListener("click", () =>
  sendCommand(commands.turnCancel(state.sessionId, state.turnId), "cancel"));

function decide(decision) {
  if (!pendingApproval) return;
  sendCommand({
    type: "approval_respond",
    params: {
      session_id: pendingApproval.session_id,
      approval_id: pendingApproval.approval_id,
      approval_subject_digest: pendingApproval.approval_subject_digest,
      decision
    }
  }, "approval");
}
approveButton.addEventListener("click", () => decide("approve"));
denyButton.addEventListener("click", () => decide("deny"));

renderIdentity();
if ("serviceWorker" in navigator) navigator.serviceWorker.register("/sw.js");
