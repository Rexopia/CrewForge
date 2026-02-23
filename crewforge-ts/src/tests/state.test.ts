import assert from "node:assert";
import { test } from "node:test";

import { ChatStateStore } from "../chat/state";

test("ChatStateStore captures startup metadata and agent summaries", () => {
  const store = new ChatStateStore();
  store.applyRpcEvent({
    type: "session.started",
    sessionId: "session-1",
    sessionFile: ".room/sessions/session-1.jsonl",
    human: "Rex",
    resumedFrom: undefined,
  });
  store.applyRpcEvent({
    type: "line",
    role: "system",
    text: "Agents: Codex[gpt-5], Kimi[kimi-k2]",
  });
  store.applyRpcEvent({
    type: "line",
    role: "agent",
    speaker: "Codex",
    ts: "12:00",
    text: "hello",
    agentIdx: 0,
  });

  const state = store.toState();
  assert.strictEqual(state.sessionId, "session-1");
  assert.strictEqual(state.human, "Rex");
  assert.strictEqual(state.agents.length, 2);
  assert.strictEqual(state.agents[0]?.name, "Codex");
  assert.strictEqual(state.agents[0]?.model, "gpt-5");
  assert.strictEqual(state.agents[0]?.posts, 1);
});

test("ChatStateStore records warnings and resume hint", () => {
  const store = new ChatStateStore();
  store.applyRpcEvent({
    type: "warning",
    message: "profile Kimi was removed",
  });
  store.applyRpcEvent({
    type: "session.resume_hint",
    command: "crewforge chat --resume session-1",
  });

  const state = store.toState();
  assert.strictEqual(state.resumeHint, "crewforge chat --resume session-1");
  assert.ok(state.lines.some((line) => line.text.includes("[warning]")));
  assert.ok(state.lines.some((line) => line.text.includes("Resume:")));
});

test("ChatStateStore follows explicit agent.status events", () => {
  const store = new ChatStateStore();
  store.applyRpcEvent({
    type: "line",
    role: "system",
    text: "Agents: Codex[gpt-5], Kimi[kimi-k2]",
  });
  store.applyRpcEvent({
    type: "line",
    role: "agent",
    speaker: "Codex",
    ts: "12:00",
    text: "hello",
    agentIdx: 0,
  });

  let state = store.toState();
  assert.strictEqual(state.agents[0]?.status, "idle");

  store.applyRpcEvent({
    type: "agent.status",
    agent: "Codex",
    status: "active",
  });
  state = store.toState();
  assert.strictEqual(state.agents[0]?.status, "active");

  store.applyRpcEvent({
    type: "agent.status",
    agent: "Codex",
    status: "idle",
  });
  state = store.toState();
  assert.strictEqual(state.agents[0]?.status, "idle");
});

test("ChatStateStore applies agents.snapshot metadata", () => {
  const store = new ChatStateStore();
  store.applyRpcEvent({
    type: "agents.snapshot",
    agents: [
      {
        name: "Codex",
        model: "gpt-5",
        contextDir: ".room/agents/codex",
        agentIdx: 0,
      },
    ],
  });

  const state = store.toState();
  assert.strictEqual(state.agents.length, 1);
  assert.strictEqual(state.agents[0]?.name, "Codex");
  assert.strictEqual(state.agents[0]?.model, "gpt-5");
  assert.strictEqual(state.agents[0]?.contextDir, ".room/agents/codex");
  assert.strictEqual(state.agents[0]?.status, "idle");
});

test("ChatStateStore parses multiline startup agents banner", () => {
  const store = new ChatStateStore();
  store.applyRpcEvent({
    type: "line",
    role: "system",
    text: "Agents:\n  Kimi[kimi-for-coding/k2p5]\n  GLM[zhipuai-coding-plan/glm-5]",
  });

  const state = store.toState();
  assert.strictEqual(state.agents.length, 2);
  assert.strictEqual(state.agents[0]?.name, "Kimi");
  assert.strictEqual(state.agents[0]?.model, "kimi-for-coding/k2p5");
  assert.strictEqual(state.agents[1]?.name, "GLM");
  assert.strictEqual(state.agents[1]?.model, "zhipuai-coding-plan/glm-5");
});
