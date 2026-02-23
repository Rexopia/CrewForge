import assert from "node:assert";
import { test } from "node:test";

import { encodeInputCommand, shouldUseChatUi } from "../chat/run-chat-ui";

test("encodeInputCommand emits jsonl payload", () => {
  assert.strictEqual(
    encodeInputCommand("hello"),
    "{\"type\":\"input\",\"text\":\"hello\"}\n",
  );
});

test("shouldUseChatUi disables UI when --rpc is explicit", () => {
  assert.strictEqual(shouldUseChatUi(["chat", "--rpc", "jsonl"]), false);
});
