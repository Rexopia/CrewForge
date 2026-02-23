export type RpcCommand =
  | { type: "input"; text: string }
  | { type: "slash_command"; command: string }
  | { type: "resize"; width: number; height: number }
  | { type: "ping" }
  | { type: "exit" };

export type RpcEvent =
  | {
      type: "session.started";
      sessionId: string;
      sessionFile: string;
      human: string;
      resumedFrom?: string | null;
    }
  | {
      type: "agents.snapshot";
      agents: Array<{
        name: string;
        model?: string;
        contextDir?: string;
        agentIdx?: number;
      }>;
    }
  | {
      type: "line";
      role: "system" | "human" | "agent";
      text: string;
      ts?: string;
      speaker?: string;
      agentIdx?: number;
    }
  | { type: "warning"; message: string }
  | { type: "error"; message: string }
  | { type: "agent.status"; agent: string; status: string; reason?: string }
  | { type: "pong" }
  | { type: "session.resume_hint"; command: string }
  | { type: "session.ended"; reason?: string };

type JsonRecord = Record<string, unknown>;

function isRecord(value: unknown): value is JsonRecord {
  return typeof value === "object" && value !== null;
}

function expectString(record: JsonRecord, key: string): string {
  const value = record[key];
  if (typeof value !== "string") {
    throw new Error(`rpc event field "${key}" must be string`);
  }
  return value;
}

function optionalString(record: JsonRecord, key: string): string | undefined {
  const value = record[key];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "string") {
    throw new Error(`rpc event field "${key}" must be string when present`);
  }
  return value;
}

function optionalNumber(record: JsonRecord, key: string): number | undefined {
  const value = record[key];
  if (value === undefined || value === null) {
    return undefined;
  }
  if (typeof value !== "number" || !Number.isFinite(value)) {
    throw new Error(`rpc event field "${key}" must be number when present`);
  }
  return value;
}

function expectArray(record: JsonRecord, key: string): unknown[] {
  const value = record[key];
  if (!Array.isArray(value)) {
    throw new Error(`rpc event field "${key}" must be array`);
  }
  return value;
}

function parseAgentSnapshot(value: unknown): {
  name: string;
  model?: string;
  contextDir?: string;
  agentIdx?: number;
} {
  if (!isRecord(value)) {
    throw new Error("rpc event field \"agents\" must contain objects");
  }
  return {
    name: expectString(value, "name"),
    model: optionalString(value, "model"),
    contextDir: optionalString(value, "contextDir"),
    agentIdx: optionalNumber(value, "agentIdx"),
  };
}

export function serializeRpcCommand(command: RpcCommand): string {
  return `${JSON.stringify(command)}\n`;
}

export function parseRpcEventLine(rawLine: string): RpcEvent {
  let parsed: unknown;
  try {
    parsed = JSON.parse(rawLine);
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    throw new Error(`invalid rpc json event: ${message}`);
  }

  if (!isRecord(parsed)) {
    throw new Error("rpc event must be a JSON object");
  }

  const eventType = expectString(parsed, "type");
  switch (eventType) {
    case "session.started":
      return {
        type: "session.started",
        sessionId: expectString(parsed, "sessionId"),
        sessionFile: expectString(parsed, "sessionFile"),
        human: expectString(parsed, "human"),
        resumedFrom: optionalString(parsed, "resumedFrom"),
      };
    case "agents.snapshot":
      return {
        type: "agents.snapshot",
        agents: expectArray(parsed, "agents").map(parseAgentSnapshot),
      };
    case "line": {
      const role = expectString(parsed, "role");
      if (role !== "system" && role !== "human" && role !== "agent") {
        throw new Error(`unsupported line role: ${role}`);
      }
      return {
        type: "line",
        role,
        text: expectString(parsed, "text"),
        ts: optionalString(parsed, "ts"),
        speaker: optionalString(parsed, "speaker"),
        agentIdx: optionalNumber(parsed, "agentIdx"),
      };
    }
    case "warning":
      return {
        type: "warning",
        message: expectString(parsed, "message"),
      };
    case "error":
      return {
        type: "error",
        message: expectString(parsed, "message"),
      };
    case "agent.status":
      return {
        type: "agent.status",
        agent: expectString(parsed, "agent"),
        status: expectString(parsed, "status"),
        reason: optionalString(parsed, "reason"),
      };
    case "pong":
      return { type: "pong" };
    case "session.resume_hint":
      return {
        type: "session.resume_hint",
        command: expectString(parsed, "command"),
      };
    case "session.ended":
      return {
        type: "session.ended",
        reason: optionalString(parsed, "reason"),
      };
    default:
      throw new Error(`unsupported rpc event type: ${eventType}`);
  }
}
