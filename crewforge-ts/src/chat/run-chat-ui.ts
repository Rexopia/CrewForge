import { spawn } from "node:child_process";
import { once } from "node:events";
import os from "node:os";
import readline from "node:readline";

import { RpcCommand, parseRpcEventLine, serializeRpcCommand } from "./protocol";
import { ChatStateStore } from "./state";
import { ChatUi } from "./ui";

type ChildResult =
  | { type: "code"; exitCode: number }
  | { type: "signal"; signal: NodeJS.Signals };

function exitCodeFromSignal(signal: NodeJS.Signals): number {
  const signalNumber = os.constants.signals[signal];
  return typeof signalNumber === "number" ? 128 + signalNumber : 1;
}

function hasRpcFlag(args: string[]): boolean {
  for (let index = 0; index < args.length; index += 1) {
    if (args[index] !== "--rpc") {
      continue;
    }
    return true;
  }
  return false;
}

function withJsonlRpcFlag(args: string[]): string[] {
  if (hasRpcFlag(args)) {
    return [...args];
  }
  return [...args, "--rpc", "jsonl"];
}

export function shouldIgnoreChildStdinError(error: NodeJS.ErrnoException): boolean {
  return error.code === "EPIPE" || error.code === "ERR_STREAM_DESTROYED";
}

export async function runChatWithUi(binary: string, rawArgs: string[]): Promise<number> {
  const childArgs = withJsonlRpcFlag(rawArgs);
  const child = spawn(binary, childArgs, {
    stdio: ["pipe", "pipe", "pipe"],
    env: process.env,
  });

  const state = new ChatStateStore();
  let forceExitTimer: NodeJS.Timeout | undefined;

  const handleStdinError = (error: NodeJS.ErrnoException): void => {
    if (shouldIgnoreChildStdinError(error)) {
      return;
    }
    const message = error.message ?? String(error);
    state.appendClientNotice(`[input pipe] ${message}`);
  };
  child.stdin?.on("error", handleStdinError);

  const sendCommand = (command: RpcCommand): void => {
    if (!child.stdin || child.stdin.destroyed || !child.stdin.writable) {
      return;
    }
    child.stdin.write(serializeRpcCommand(command));
  };

  const requestExit = (): void => {
    sendCommand({ type: "exit" });
    if (forceExitTimer) {
      return;
    }
    forceExitTimer = setTimeout(() => {
      if (!child.killed) {
        child.kill("SIGTERM");
      }
    }, 2_500);
    forceExitTimer.unref();
  };

  const ui = new ChatUi({
    onSubmitInput: (text) => {
      sendCommand({ type: "input", text });
    },
    onExitRequested: requestExit,
  });

  const refreshUi = (): void => {
    ui.render(state.toState());
  };

  const stdoutReader = readline.createInterface({
    input: child.stdout,
    crlfDelay: Infinity,
  });
  const stdoutClosed = once(stdoutReader, "close");
  stdoutReader.on("line", (line: string) => {
    const payload = line.trim();
    if (!payload) {
      return;
    }
    try {
      state.applyRpcEvent(parseRpcEventLine(payload));
    } catch (error) {
      const message = error instanceof Error ? error.message : String(error);
      state.appendClientNotice(`[rpc parse error] ${message}`);
    }
    refreshUi();
  });

  const stderrReader = readline.createInterface({
    input: child.stderr,
    crlfDelay: Infinity,
  });
  const stderrClosed = once(stderrReader, "close");
  stderrReader.on("line", (line: string) => {
    state.appendCoreStderr(line);
    refreshUi();
  });

  const signalHandlers = new Map<NodeJS.Signals, () => void>();
  for (const signal of ["SIGINT", "SIGTERM", "SIGHUP"] as const) {
    const handler = () => {
      requestExit();
      if (!child.killed) {
        try {
          child.kill(signal);
        } catch {
          // Ignore races when child is already gone.
        }
      }
    };
    signalHandlers.set(signal, handler);
    process.on(signal, handler);
  }

  const resizeHandler = () => {
    const width = process.stdout.columns ?? 0;
    const height = process.stdout.rows ?? 0;
    if (width > 0 && height > 0) {
      sendCommand({ type: "resize", width, height });
    }
  };
  process.stdout.on("resize", resizeHandler);
  resizeHandler();

  const childResult = await new Promise<ChildResult>((resolve, reject) => {
    child.once("error", reject);
    child.once("close", (code, signal) => {
      if (signal) {
        resolve({ type: "signal", signal });
      } else {
        resolve({ type: "code", exitCode: code ?? 1 });
      }
    });
  }).catch((error: unknown): ChildResult => {
    const message = error instanceof Error ? error.message : String(error);
    state.appendClientNotice(`[launcher] failed to start core: ${message}`);
    refreshUi();
    return { type: "code", exitCode: 1 };
  });

  if (forceExitTimer) {
    clearTimeout(forceExitTimer);
    forceExitTimer = undefined;
  }

  process.stdout.off("resize", resizeHandler);
  for (const [signal, handler] of signalHandlers) {
    process.off(signal, handler);
  }

  await Promise.all([stdoutClosed, stderrClosed]);
  const finalState = state.toState();
  ui.destroy();

  if (finalState.resumeHint) {
    process.stdout.write(`Resume this session with: ${finalState.resumeHint}\n`);
  }

  if (childResult.type === "signal") {
    return exitCodeFromSignal(childResult.signal);
  }
  return childResult.exitCode;
}

export function shouldUseChatUi(commandArgs: string[]): boolean {
  if (!(process.stdin.isTTY && process.stdout.isTTY)) {
    return false;
  }
  if (hasRpcFlag(commandArgs)) {
    return false;
  }
  return true;
}

export function encodeInputCommand(text: string): string {
  return serializeRpcCommand({ type: "input", text });
}
