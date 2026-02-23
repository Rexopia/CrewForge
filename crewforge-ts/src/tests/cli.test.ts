import assert from "node:assert";
import { mkdtempSync, writeFileSync } from "node:fs";
import os from "node:os";
import path from "node:path";
import { spawnSync } from "node:child_process";
import { test } from "node:test";

const cliPath = path.resolve(__dirname, "..", "bin", "crewforge.js");

function writeExecutable(filePath: string, content: string): void {
  writeFileSync(filePath, content, { mode: 0o755 });
}

function runCli(args: string[], env: NodeJS.ProcessEnv): ReturnType<typeof spawnSync> {
  return spawnSync(process.execPath, [cliPath, ...args], {
    env,
    encoding: "utf8",
  });
}

test("cli forwards args to CREWFORGE_CORE_BIN when provided", () => {
  const tempDir = mkdtempSync(path.join(os.tmpdir(), "crewforge-ts-test-"));
  const fakeCore = path.join(tempDir, "fake-core.sh");

  writeExecutable(
    fakeCore,
    "#!/usr/bin/env bash\nset -euo pipefail\necho \"core:$*\"\n",
  );

  const result = runCli(["chat", "--dry-run"], {
    ...process.env,
    CREWFORGE_CORE_BIN: fakeCore,
  });

  assert.strictEqual(result.status, 0);
  assert.match(String(result.stdout), /core:chat --dry-run/);
});

test("cli propagates core exit code", () => {
  const tempDir = mkdtempSync(path.join(os.tmpdir(), "crewforge-ts-test-"));
  const fakeCore = path.join(tempDir, "fake-core.sh");

  writeExecutable(
    fakeCore,
    "#!/usr/bin/env bash\nset -euo pipefail\nexit 7\n",
  );

  const result = runCli(["chat"], {
    ...process.env,
    CREWFORGE_CORE_BIN: fakeCore,
  });

  assert.strictEqual(result.status, 7);
});

test("cli maps signaled core exits to shell-compatible exit codes", () => {
  if (process.platform === "win32") {
    return;
  }

  const tempDir = mkdtempSync(path.join(os.tmpdir(), "crewforge-ts-test-"));
  const fakeCore = path.join(tempDir, "fake-core.sh");

  writeExecutable(
    fakeCore,
    "#!/usr/bin/env bash\nset -euo pipefail\nkill -TERM $$\n",
  );

  const result = runCli(["chat"], {
    ...process.env,
    CREWFORGE_CORE_BIN: fakeCore,
  });

  assert.strictEqual(result.status, 143);
});
