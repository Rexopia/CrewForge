import { RpcEvent } from "./protocol";

const MAX_LINES = 4000;

export type UiLineRole = "system" | "human" | "agent";
export type AgentRuntimeStatus = "idle" | "active" | "error" | "unknown";

export interface UiLine {
  id: number;
  role: UiLineRole;
  text: string;
  ts?: string;
  speaker?: string;
  agentIdx?: number;
}

export interface AgentSummary {
  name: string;
  model?: string;
  contextDir?: string;
  colorIdx?: number;
  posts: number;
  lastTs?: string;
  status: AgentRuntimeStatus;
  statusReason?: string;
}

export interface ChatUiState {
  sessionId?: string;
  sessionFile?: string;
  human?: string;
  resumedFrom?: string;
  resumeHint?: string;
  started: boolean;
  ended: boolean;
  lines: UiLine[];
  agents: AgentSummary[];
  lastError?: string;
}

function ensureAgent(
  agentByName: Map<string, AgentSummary>,
  order: string[],
  name: string,
): AgentSummary {
  const normalized = name.trim();
  const existing = agentByName.get(normalized);
  if (existing) {
    return existing;
  }
  const created: AgentSummary = {
    name: normalized,
    posts: 0,
    status: "unknown",
  };
  agentByName.set(normalized, created);
  order.push(normalized);
  return created;
}

function parseAgentsStartupLine(text: string): Array<{ name: string; model?: string }> {
  if (!text.startsWith("Agents:")) {
    return [];
  }
  const payload = text.slice("Agents:".length).trim();
  if (!payload) {
    return [];
  }
  return payload
    .split(/[\n,]/)
    .map((chunk) => chunk.trim())
    .filter((chunk) => chunk.length > 0)
    .map((chunk) => {
      const match = chunk.match(/^(.+?)\[(.+)\]$/);
      if (!match) {
        return { name: chunk };
      }
      return {
        name: match[1].trim(),
        model: match[2].trim(),
      };
    })
    .filter((item) => item.name.length > 0);
}

export class ChatStateStore {
  private nextLineId = 1;
  private readonly lines: UiLine[] = [];
  private readonly agentByName = new Map<string, AgentSummary>();
  private readonly agentOrder: string[] = [];
  private sessionId?: string;
  private sessionFile?: string;
  private human?: string;
  private resumedFrom?: string;
  private resumeHint?: string;
  private started = false;
  private ended = false;
  private lastError?: string;

  applyRpcEvent(event: RpcEvent): void {
    switch (event.type) {
      case "session.started":
        this.started = true;
        this.ended = false;
        this.sessionId = event.sessionId;
        this.sessionFile = event.sessionFile;
        this.human = event.human;
        this.resumedFrom = event.resumedFrom ?? undefined;
        return;
      case "agents.snapshot":
        for (let index = 0; index < event.agents.length; index += 1) {
          const item = event.agents[index];
          const agent = ensureAgent(this.agentByName, this.agentOrder, item.name);
          agent.model = item.model ?? agent.model;
          agent.contextDir = item.contextDir ?? agent.contextDir;
          if (typeof item.agentIdx === "number") {
            agent.colorIdx = item.agentIdx;
          } else if (agent.colorIdx === undefined) {
            agent.colorIdx = index;
          }
          if (agent.status === "unknown") {
            agent.status = "idle";
          }
        }
        return;
      case "line":
        this.pushLine({
          role: event.role,
          text: event.text,
          ts: event.ts,
          speaker: event.speaker,
          agentIdx: event.agentIdx,
        });
        this.updateAgentsFromLine(event.role, event.text, event.speaker, event.ts, event.agentIdx);
        return;
      case "warning":
        this.pushSystem(`[warning] ${event.message}`);
        return;
      case "error":
        this.lastError = event.message;
        this.pushSystem(`[error] ${event.message}`);
        return;
      case "agent.status": {
        const agent = ensureAgent(this.agentByName, this.agentOrder, event.agent);
        agent.status = mapAgentStatus(event.status);
        agent.statusReason = event.reason;
        return;
      }
      case "pong":
        return;
      case "session.resume_hint":
        this.resumeHint = event.command;
        this.pushSystem(`Resume: ${event.command}`);
        return;
      case "session.ended":
        this.ended = true;
        if (event.reason) {
          this.pushSystem(`Session ended: ${event.reason}`);
        }
        return;
    }
  }

  appendCoreStderr(text: string): void {
    const normalized = text.trim();
    if (!normalized) {
      return;
    }
    this.pushSystem(`[core] ${normalized}`);
    if (normalized.includes("[update]")) {
      this.lastError = undefined;
    }
  }

  appendClientNotice(text: string): void {
    const normalized = text.trim();
    if (!normalized) {
      return;
    }
    this.pushSystem(normalized);
  }

  toState(): ChatUiState {
    return {
      sessionId: this.sessionId,
      sessionFile: this.sessionFile,
      human: this.human,
      resumedFrom: this.resumedFrom,
      resumeHint: this.resumeHint,
      started: this.started,
      ended: this.ended,
      lines: [...this.lines],
      agents: this.agentOrder
        .map((name) => this.agentByName.get(name))
        .filter((item): item is AgentSummary => Boolean(item))
        .map((item) => ({ ...item })),
      lastError: this.lastError,
    };
  }

  private pushSystem(text: string): void {
    this.pushLine({
      role: "system",
      text,
    });
  }

  private pushLine(input: Omit<UiLine, "id">): void {
    this.lines.push({
      id: this.nextLineId++,
      ...input,
    });
    while (this.lines.length > MAX_LINES) {
      this.lines.shift();
    }
  }

  private updateAgentsFromLine(
    role: UiLineRole,
    text: string,
    speaker: string | undefined,
    ts: string | undefined,
    agentIdx: number | undefined,
  ): void {
    if (role === "agent" && speaker) {
      const agent = ensureAgent(this.agentByName, this.agentOrder, speaker);
      agent.posts += 1;
      agent.lastTs = ts ?? agent.lastTs;
      if (typeof agentIdx === "number") {
        agent.colorIdx = agentIdx;
      }
    }

    if (role !== "system") {
      return;
    }

    const startupAgents = parseAgentsStartupLine(text);
    for (let index = 0; index < startupAgents.length; index += 1) {
      const item = startupAgents[index];
      const agent = ensureAgent(this.agentByName, this.agentOrder, item.name);
      agent.model = item.model ?? agent.model;
      if (agent.colorIdx === undefined) {
        agent.colorIdx = index;
      }
      if (agent.status === "unknown") {
        agent.status = "idle";
      }
    }

  }
}

function mapAgentStatus(status: string): AgentRuntimeStatus {
  const normalized = status.trim().toLowerCase();
  if (normalized === "idle" || normalized === "active" || normalized === "error") {
    return normalized;
  }
  return "unknown";
}
