import assert from "node:assert";
import { test } from "node:test";

import { parseRpcEventLine, serializeRpcCommand } from "../chat/protocol";

test("serializeRpcCommand appends newline for wire format", () => {
  const encoded = serializeRpcCommand({ type: "input", text: "hello" });
  assert.strictEqual(encoded, "{\"type\":\"input\",\"text\":\"hello\"}\n");
});

test("parseRpcEventLine parses session.started", () => {
  const event = parseRpcEventLine(
    "{\"type\":\"session.started\",\"sessionId\":\"s1\",\"sessionFile\":\".room/sessions/s1.jsonl\",\"human\":\"Rex\"}",
  );
  assert.deepStrictEqual(event, {
    type: "session.started",
    sessionId: "s1",
    sessionFile: ".room/sessions/s1.jsonl",
    human: "Rex",
    resumedFrom: undefined,
  });
});

test("parseRpcEventLine parses line event", () => {
  const event = parseRpcEventLine(
    "{\"type\":\"line\",\"role\":\"agent\",\"ts\":\"12:00:00\",\"speaker\":\"Codex\",\"text\":\"hi\",\"agentIdx\":1}",
  );
  assert.deepStrictEqual(event, {
    type: "line",
    role: "agent",
    ts: "12:00:00",
    speaker: "Codex",
    text: "hi",
    agentIdx: 1,
  });
});

test("parseRpcEventLine parses agents.snapshot event", () => {
  const event = parseRpcEventLine(
    "{\"type\":\"agents.snapshot\",\"agents\":[{\"name\":\"Codex\",\"model\":\"gpt-5\",\"contextDir\":\".room/agents/codex\",\"agentIdx\":0}]}",
  );
  assert.deepStrictEqual(event, {
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
});

test("parseRpcEventLine rejects unsupported event type", () => {
  assert.throws(
    () => parseRpcEventLine("{\"type\":\"unknown\"}"),
    /unsupported rpc event type/,
  );
});
