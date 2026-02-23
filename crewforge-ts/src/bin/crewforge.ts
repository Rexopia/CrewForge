#!/usr/bin/env node

import { existsSync } from "node:fs";
import path from "node:path";
import { spawn } from "node:child_process";
import os from "node:os";
import { runChatWithUi, shouldUseChatUi } from "../chat/run-chat-ui";

const PACKAGES: Record<string, string> = {
  "linux-x64": "@crewforge/core-linux-x64",
  "linux-arm64": "@crewforge/core-linux-arm64",
  "darwin-x64": "@crewforge/core-darwin-x64",
  "darwin-arm64": "@crewforge/core-darwin-arm64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = PACKAGES[key];

if (!pkg) {
  process.stderr.write(`[crewforge] Unsupported platform: ${key}\n`);
  process.exit(1);
}

const binName = process.platform === "win32" ? "crewforge.exe" : "crewforge";

function localDevCoreBinary(): string | null {
  const candidates = [
    path.resolve(__dirname, "..", "..", "..", "crewforge-rs", "target", "debug", binName),
    path.resolve(__dirname, "..", "..", "..", "target", "debug", binName),
  ];
  for (const candidate of candidates) {
    if (existsSync(candidate)) {
      return candidate;
    }
  }
  return null;
}

function resolveCoreBinary(): string | null {
  const envBinary = process.env.CREWFORGE_CORE_BIN;
  if (envBinary && existsSync(envBinary)) {
    return envBinary;
  }

  const localBinary = localDevCoreBinary();
  if (localBinary) {
    return localBinary;
  }

  try {
    // eslint-disable-next-line @typescript-eslint/no-var-requires
    return require.resolve(`${pkg}/${binName}`);
  } catch {
    process.stderr.write(
      `[crewforge] Core binary not found for ${key} (${pkg}).\n` +
        "Install or reinstall: npm i -g crewforge\n" +
        "For local development run: cargo build --manifest-path crewforge-rs/Cargo.toml\n",
    );
    return null;
  }
}

type ChildResult = { type: "signal"; signal: NodeJS.Signals } | { type: "code"; exitCode: number };

function exitCodeFromSignal(signal: NodeJS.Signals): number {
  const signalNumber = os.constants.signals[signal];
  return typeof signalNumber === "number" ? 128 + signalNumber : 1;
}

async function runCoreInherit(binary: string, args: string[]): Promise<number> {
  const child = spawn(binary, args, { stdio: "inherit" });
  const forwardedSignals: NodeJS.Signals[] = ["SIGINT", "SIGTERM", "SIGHUP"];
  const signalHandlers = new Map<NodeJS.Signals, () => void>();

  for (const signal of forwardedSignals) {
    const handler = () => {
      if (child.killed) {
        return;
      }
      try {
        child.kill(signal);
      } catch {
        // Ignore races where child exits before we forward the signal.
      }
    };
    signalHandlers.set(signal, handler);
    process.on(signal, handler);
  }

  child.once("error", (error: Error) => {
    process.stderr.write(`[crewforge] Failed to launch core binary: ${error.message}\n`);
    for (const [signal, handler] of signalHandlers) {
      process.off(signal, handler);
    }
    process.exit(1);
  });

  const result = await new Promise<ChildResult>((resolve) => {
    child.once("exit", (code, signal) => {
      if (signal) {
        resolve({ type: "signal", signal });
      } else {
        resolve({ type: "code", exitCode: code ?? 1 });
      }
    });
  });

  for (const [signal, handler] of signalHandlers) {
    process.off(signal, handler);
  }

  if (result.type === "signal") {
    return exitCodeFromSignal(result.signal);
  }

  return result.exitCode;
}

async function run(): Promise<number> {
  const binaryPath = resolveCoreBinary();
  if (!binaryPath) {
    return 1;
  }

  const args = process.argv.slice(2);
  const command = args[0];
  if (command === "chat" && shouldUseChatUi(args)) {
    return runChatWithUi(binaryPath, args);
  }

  return runCoreInherit(binaryPath, args);
}

run()
  .then((exitCode) => {
    process.exit(exitCode);
  })
  .catch((error: unknown) => {
    const message = error instanceof Error ? error.message : String(error);
    process.stderr.write(`[crewforge] ${message}\n`);
    process.exit(1);
  });
