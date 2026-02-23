import assert from "node:assert";
import { test } from "node:test";

import { __uiTestHelpers } from "../chat/ui";

test("wrappedLinesWithCursor wraps by display width for CJK input", () => {
  const chars = Array.from("ab你cd");
  const wrapped = __uiTestHelpers.wrappedLinesWithCursor(chars, 3, 4);
  assert.deepStrictEqual(wrapped.lines, ["ab你", "cd"]);
  assert.strictEqual(wrapped.cursorLine, 1);
  assert.strictEqual(wrapped.cursorCol, 0);
});

test("wrappedLinesWithCursor keeps cursor column as character index", () => {
  const chars = Array.from("你a");
  const wrapped = __uiTestHelpers.wrappedLinesWithCursor(chars, 1, 6);
  assert.strictEqual(wrapped.cursorLine, 0);
  assert.strictEqual(wrapped.cursorCol, 1);
});
