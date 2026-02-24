import assert from "node:assert";
import { test } from "node:test";

import {
  encodeInputCommand,
  shouldIgnoreChildStdinError,
  shouldUseChatUi,
} from "../chat/run-chat-ui";

test("encodeInputCommand emits jsonl payload", () => {
  assert.strictEqual(
    encodeInputCommand("hello"),
    "{\"type\":\"input\",\"text\":\"hello\"}\n",
  );
});

test("shouldUseChatUi disables UI when --rpc is explicit", () => {
  assert.strictEqual(shouldUseChatUi(["chat", "--rpc", "jsonl"]), false);
});

test("shouldIgnoreChildStdinError only suppresses expected pipe races", () => {
  assert.strictEqual(
    shouldIgnoreChildStdinError({ code: "EPIPE" } as NodeJS.ErrnoException),
    true,
  );
  assert.strictEqual(
    shouldIgnoreChildStdinError({ code: "ERR_STREAM_DESTROYED" } as NodeJS.ErrnoException),
    true,
  );
  assert.strictEqual(
    shouldIgnoreChildStdinError({ code: "EINVAL" } as NodeJS.ErrnoException),
    false,
  );
});
