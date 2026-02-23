import blessed from "blessed";

import { AgentSummary, ChatUiState, UiLine } from "./state";

export interface ChatUiHandlers {
  onSubmitInput: (text: string) => void;
  onExitRequested: () => void;
}

function escapeTags(text: string): string {
  return text.replaceAll("{", "\\{").replaceAll("}", "\\}");
}

function colorForAgentIndex(agentIdx: number | undefined): string {
  if (agentIdx === undefined) {
    return "green";
  }
  const palette = ["green", "yellow", "magenta", "blue", "red"];
  return palette[Math.abs(agentIdx) % palette.length];
}

const MESSAGE_FRAME_MAX_COLS = 88;
const INPUT_MIN_VISIBLE_ROWS = 1;
const INPUT_MAX_VISIBLE_ROWS = 3;
const INPUT_CHROME_ROWS = 2;

function charDisplayWidth(ch: string): number {
  const unicode = (blessed as unknown as { unicode?: { charWidth?: (value: string) => number } }).unicode;
  const width = unicode?.charWidth?.(ch);
  if (typeof width !== "number" || !Number.isFinite(width) || width <= 0) {
    return 1;
  }
  return width;
}

function strDisplayWidth(text: string): number {
  const unicode = (blessed as unknown as { unicode?: { strWidth?: (value: string) => number } }).unicode;
  const width = unicode?.strWidth?.(text);
  if (typeof width !== "number" || !Number.isFinite(width) || width < 0) {
    return Array.from(text).length;
  }
  return width;
}

function truncateToDisplayWidth(text: string, maxCols: number): string {
  if (maxCols <= 0) {
    return "";
  }
  if (strDisplayWidth(text) <= maxCols) {
    return text;
  }
  if (maxCols === 1) {
    return "…";
  }
  const budget = maxCols - 1;
  let width = 0;
  const out: string[] = [];
  for (const ch of Array.from(text)) {
    const w = charDisplayWidth(ch);
    if (width + w > budget) {
      break;
    }
    out.push(ch);
    width += w;
  }
  return `${out.join("")}…`;
}

function wrapPlainText(text: string, maxCols: number): string[] {
  const widthLimit = Math.max(1, maxCols);
  const normalized = text.replaceAll("\r\n", "\n").replaceAll("\r", "\n");
  const sourceLines = normalized.split("\n");
  const output: string[] = [];
  for (const sourceLine of sourceLines) {
    if (sourceLine.length === 0) {
      output.push("");
      continue;
    }
    output.push(...wrapLineWordAware(sourceLine, widthLimit));
  }
  if (output.length === 0) {
    return [""];
  }
  return output;
}

function wrapLineWordAware(line: string, widthLimit: number): string[] {
  const chars = Array.from(line);
  const wrapped: string[] = [];
  let start = 0;

  while (start < chars.length) {
    let width = 0;
    let end = start;
    let lastWhitespace = -1;

    while (end < chars.length) {
      const ch = chars[end];
      const chWidth = charDisplayWidth(ch);
      if (width + chWidth > widthLimit) {
        break;
      }
      width += chWidth;
      end += 1;
      if (/\s/.test(ch)) {
        lastWhitespace = end;
      }
    }

    if (end >= chars.length) {
      wrapped.push(chars.slice(start).join(""));
      break;
    }

    let breakAt = end;
    if (lastWhitespace > start) {
      breakAt = lastWhitespace;
    }

    let chunk = chars.slice(start, breakAt).join("");
    chunk = chunk.replace(/\s+$/g, "");
    if (chunk.length === 0) {
      // Fallback for long uninterrupted tokens.
      breakAt = Math.max(start + 1, end);
      chunk = chars.slice(start, breakAt).join("");
    }
    wrapped.push(chunk);

    start = breakAt;
    while (start < chars.length && /\s/.test(chars[start])) {
      start += 1;
    }
  }

  if (wrapped.length === 0) {
    wrapped.push("");
  }
  return wrapped;
}

function formatMessageLines(line: UiLine, bodyMaxCols: number): string[] {
  const wrappedTextLines = wrapPlainText(line.text, bodyMaxCols).map(escapeTags);
  if (line.role === "system") {
    return wrappedTextLines.map((text) => `{gray-fg}${text}{/gray-fg}`);
  }

  const prefixParts = [];
  if (line.ts) {
    prefixParts.push(`[${line.ts}]`);
  }
  if (line.speaker) {
    prefixParts.push(line.speaker);
  }
  const prefix = prefixParts.join(" ");
  const title = truncateToDisplayWidth(prefix || (line.role === "human" ? "Human" : "Agent"), bodyMaxCols);

  if (line.role === "human") {
    return [`{cyan-fg}${escapeTags(title)}{/cyan-fg}`, ...wrappedTextLines, ""];
  }

  const color = colorForAgentIndex(line.agentIdx);
  return [`{${color}-fg}${escapeTags(title)}{/${color}-fg}`, ...wrappedTextLines, ""];
}

function statusDotColor(status: AgentSummary["status"]): string {
  if (status === "active") {
    return "yellow";
  }
  if (status === "error") {
    return "red";
  }
  return "gray";
}

function renderStatusDot(status: AgentSummary["status"]): string {
  const color = statusDotColor(status);
  const dot = `{${color}-fg}●{/${color}-fg}`;
  if (status === "active") {
    return `{blink}${dot}{/blink}`;
  }
  return dot;
}

export class ChatUi {
  private readonly screen: blessed.Widgets.Screen;
  private readonly messagesBox: blessed.Widgets.BoxElement;
  private readonly inputBox: blessed.Widgets.BoxElement;
  private readonly agentStatusBar: blessed.Widgets.BoxElement;
  private readonly handlers: ChatUiHandlers;
  private autoScroll = true;
  private destroyed = false;
  private inputValue = "";
  private inputCursor = 0;
  private inputVisibleRowCount = INPUT_MIN_VISIBLE_ROWS;

  constructor(handlers: ChatUiHandlers) {
    this.handlers = handlers;
    this.screen = blessed.screen({
      smartCSR: true,
      dockBorders: true,
      title: "CrewForge",
      fullUnicode: true,
      autoPadding: true,
    });

    this.messagesBox = blessed.box({
      parent: this.screen,
      top: 0,
      left: 0,
      width: "100%",
      height: "100%-3",
      border: "line",
      tags: true,
      scrollable: true,
      alwaysScroll: false,
      mouse: false,
      keys: true,
      vi: true,
      style: {
        border: { fg: "#4d5f7a" },
      },
      label: " Room ",
    });

    this.inputBox = blessed.box({
      parent: this.screen,
      top: "100%-4",
      left: 0,
      width: "100%",
      height: 3,
      border: "line",
      mouse: false,
      keys: true,
      tags: true,
      scrollable: false,
      style: {
        border: { fg: "#4d5f7a" },
      },
      label: " Human ",
    });

    this.agentStatusBar = blessed.box({
      parent: this.screen,
      top: "100%-1",
      left: 0,
      width: "100%",
      height: 1,
      tags: true,
      style: {
        fg: "white",
      },
      content: "",
    });

    this.installKeyBindings();
    this.inputBox.focus();
    this.applyInputLayout(this.inputVisibleRowCount);
    this.refreshInputBox();
    this.screen.on("resize", () => {
      this.refreshInputBox();
      this.screen.render();
    });
    this.screen.render();
  }

  render(state: ChatUiState): void {
    if (this.destroyed) {
      return;
    }

    this.inputBox.setLabel(` ${state.human ?? "Human"} `);
    this.refreshInputBox();

    const bodyMaxCols = this.messageBodyMaxCols();
    const lineContent = state.lines
      .flatMap((line) => formatMessageLines(line, bodyMaxCols))
      .join("\n");
    const shouldStickBottom = this.autoScroll || this.isRoomNearBottom();
    this.messagesBox.setContent(lineContent);
    if (shouldStickBottom) {
      this.scrollRoomToBottom();
      this.autoScroll = true;
    }

    this.agentStatusBar.setContent(buildAgentStatusStrip(state, this.agentStatusBarWidth()));
    this.screen.render();
  }

  pushLocalLine(message: string): void {
    if (this.destroyed) {
      return;
    }
    const escaped = escapeTags(message);
    const current = this.messagesBox.getContent();
    const next = current ? `${current}\n{gray-fg}${escaped}{/gray-fg}` : `{gray-fg}${escaped}{/gray-fg}`;
    this.messagesBox.setContent(next);
    if (this.autoScroll) {
      this.scrollRoomToBottom();
    }
    this.screen.render();
  }

  destroy(): void {
    if (this.destroyed) {
      return;
    }
    this.destroyed = true;
    this.screen.destroy();
  }

  private installKeyBindings(): void {
    const clearInput = (): boolean => {
      this.inputValue = "";
      this.inputCursor = 0;
      this.wakeCursor();
      this.screen.render();
      return false;
    };

    const requestExit = (): boolean => {
      this.handlers.onExitRequested();
      return false;
    };

    const scrollUp = (): boolean => {
      this.autoScroll = false;
      this.messagesBox.scroll(-3);
      this.screen.render();
      return false;
    };

    const scrollDown = (): boolean => {
      this.messagesBox.scroll(3);
      this.autoScroll = this.isRoomNearBottom();
      this.screen.render();
      return false;
    };

    const jumpTop = (): boolean => {
      this.autoScroll = false;
      this.messagesBox.setScroll(0);
      this.screen.render();
      return false;
    };

    const jumpBottom = (): boolean => {
      this.autoScroll = true;
      this.scrollRoomToBottom();
      this.screen.render();
      return false;
    };

    this.screen.key(["C-d"], () => requestExit());
    this.inputBox.key(["C-d"], () => requestExit());

    this.screen.key(["C-c"], () => {
      if (this.screen.focused === this.inputBox) {
        return clearInput();
      }
      return requestExit();
    });
    this.inputBox.key(["C-c"], () => clearInput());

    this.inputBox.key(["enter"], () => {
      const text = this.inputValue.trim();
      this.inputValue = "";
      this.inputCursor = 0;
      this.autoScroll = true;
      this.wakeCursor();
      if (text.length > 0) {
        this.handlers.onSubmitInput(text);
      }
      this.screen.render();
      return false;
    });

    this.inputBox.key(["C-j", "linefeed"], () => {
      this.insertInputText("\n");
      this.screen.render();
      return false;
    });

    this.inputBox.key(["left"], () => {
      if (this.inputCursor > 0) {
        this.inputCursor -= 1;
        this.wakeCursor();
        this.screen.render();
      }
      return false;
    });

    this.inputBox.key(["right"], () => {
      const max = Array.from(this.inputValue).length;
      if (this.inputCursor < max) {
        this.inputCursor += 1;
        this.wakeCursor();
        this.screen.render();
      }
      return false;
    });

    this.inputBox.key(["up"], () => {
      this.moveCursorVertical(-1);
      this.wakeCursor();
      this.screen.render();
      return false;
    });

    this.inputBox.key(["down"], () => {
      this.moveCursorVertical(1);
      this.wakeCursor();
      this.screen.render();
      return false;
    });

    this.inputBox.key(["home"], () => {
      this.moveCursorToLineBoundary("start");
      this.wakeCursor();
      this.screen.render();
      return false;
    });

    this.inputBox.key(["end"], () => {
      this.moveCursorToLineBoundary("end");
      this.wakeCursor();
      this.screen.render();
      return false;
    });

    this.inputBox.key(["backspace"], () => {
      this.deleteBackward();
      this.wakeCursor();
      this.screen.render();
      return false;
    });

    this.inputBox.key(["delete"], () => {
      this.deleteForward();
      this.wakeCursor();
      this.screen.render();
      return false;
    });

    this.inputBox.on("keypress", (ch: string, key: blessed.Widgets.Events.IKeyEventArg) => {
      if (!ch) {
        return;
      }
      if (!key || key.ctrl || key.meta) {
        return;
      }
      if (/^[\x00-\x1f\x7f]$/.test(ch)) {
        return;
      }
      this.insertInputText(ch);
      this.screen.render();
    });

    this.screen.key(["tab"], () => {
      this.inputBox.focus();
      this.screen.render();
    });

    this.screen.key(["pageup"], () => scrollUp());
    this.inputBox.key(["pageup"], () => scrollUp());

    this.screen.key(["pagedown"], () => scrollDown());
    this.inputBox.key(["pagedown"], () => scrollDown());

    this.screen.key(["home"], () => jumpTop());

    this.screen.key(["end"], () => jumpBottom());
  }

  private wakeCursor(): void {
    this.refreshInputBox();
  }

  private insertInputText(text: string): void {
    const chars = Array.from(this.inputValue);
    chars.splice(this.inputCursor, 0, ...Array.from(text));
    this.inputValue = chars.join("");
    this.inputCursor += Array.from(text).length;
    this.wakeCursor();
  }

  private deleteBackward(): void {
    if (this.inputCursor <= 0) {
      return;
    }
    const chars = Array.from(this.inputValue);
    chars.splice(this.inputCursor - 1, 1);
    this.inputValue = chars.join("");
    this.inputCursor -= 1;
  }

  private deleteForward(): void {
    const chars = Array.from(this.inputValue);
    if (this.inputCursor >= chars.length) {
      return;
    }
    chars.splice(this.inputCursor, 1);
    this.inputValue = chars.join("");
  }

  private moveCursorVertical(direction: -1 | 1): void {
    const chars = Array.from(this.inputValue);
    const ranges = lineRanges(chars);
    const pos = cursorLogicalPosition(this.inputCursor, ranges);
    const targetLine = pos.line + direction;
    if (targetLine < 0 || targetLine >= ranges.length) {
      return;
    }
    const range = ranges[targetLine];
    this.inputCursor = Math.min(range.start + pos.col, range.end);
  }

  private moveCursorToLineBoundary(which: "start" | "end"): void {
    const chars = Array.from(this.inputValue);
    const ranges = lineRanges(chars);
    const pos = cursorLogicalPosition(this.inputCursor, ranges);
    const range = ranges[pos.line];
    this.inputCursor = which === "start" ? range.start : range.end;
  }

  private refreshInputBox(): void {
    const contentWidth = this.inputContentWidth();
    const chars = Array.from(this.inputValue);
    const clampedCursor = Math.max(0, Math.min(this.inputCursor, chars.length));
    this.inputCursor = clampedCursor;

    const wrapped = wrappedLinesWithCursor(chars, clampedCursor, contentWidth);
    const desiredVisibleRows = Math.max(
      INPUT_MIN_VISIBLE_ROWS,
      Math.min(INPUT_MAX_VISIBLE_ROWS, wrapped.lines.length || 1),
    );
    if (desiredVisibleRows !== this.inputVisibleRowCount) {
      this.applyInputLayout(desiredVisibleRows);
      this.inputVisibleRowCount = desiredVisibleRows;
    }
    const visibleRows = this.inputVisibleRows();
    const scrollTop = Math.max(0, wrapped.cursorLine - (visibleRows - 1));
    const lines: string[] = [];
    for (let row = scrollTop; row < scrollTop + visibleRows; row += 1) {
      const line = wrapped.lines[row] ?? "";
      if (row === wrapped.cursorLine) {
        lines.push(renderCursorLine(line, wrapped.cursorCol, true));
      } else {
        lines.push(escapeTags(line));
      }
    }
    this.inputBox.setContent(lines.join("\n"));
  }

  private inputContentWidth(): number {
    const rawWidth = typeof this.inputBox.width === "number" ? this.inputBox.width : this.screen.width;
    const width = Number(rawWidth);
    const screenWidth = Number(this.screen.width);
    const resolved = Number.isFinite(width) ? width : Number.isFinite(screenWidth) ? screenWidth : 80;
    return Math.max(1, resolved - 2);
  }

  private inputVisibleRows(): number {
    return this.inputVisibleRowCount;
  }

  private messageBodyMaxCols(): number {
    const rawRoomWidth =
      typeof this.messagesBox.width === "number"
        ? this.messagesBox.width
        : Number(this.screen.width);
    const roomWidth = Number(rawRoomWidth);
    const screenWidth = Number(this.screen.width);
    const resolvedRoomWidth = Number.isFinite(roomWidth)
      ? roomWidth
      : Number.isFinite(screenWidth)
        ? screenWidth
        : 80;
    const roomContentWidth = Math.max(8, resolvedRoomWidth - 2);
    return Math.max(8, Math.min(MESSAGE_FRAME_MAX_COLS, roomContentWidth));
  }

  private agentStatusBarWidth(): number {
    const rawWidth = typeof this.agentStatusBar.width === "number" ? this.agentStatusBar.width : this.screen.width;
    const width = Number(rawWidth);
    const resolved = Number.isFinite(width) ? width : Number(this.screen.width);
    return Math.max(1, Number.isFinite(resolved) ? resolved : 80);
  }

  private isRoomNearBottom(): boolean {
    const perc = this.messagesBox.getScrollPerc();
    return Number.isFinite(perc) ? perc >= 98 : true;
  }

  private scrollRoomToBottom(): void {
    this.messagesBox.setScroll(this.messagesBox.getScrollHeight());
  }

  private applyInputLayout(visibleRows: number): void {
    const rows = Math.max(INPUT_MIN_VISIBLE_ROWS, Math.min(INPUT_MAX_VISIBLE_ROWS, visibleRows));
    const totalHeight = rows + INPUT_CHROME_ROWS;
    const offset = `100%-${totalHeight + 1}`;
    this.messagesBox.height = offset;
    this.inputBox.top = offset;
    this.inputBox.height = totalHeight;
  }
}

function lineRanges(chars: string[]): Array<{ start: number; end: number }> {
  const ranges: Array<{ start: number; end: number }> = [];
  let start = 0;
  for (let index = 0; index <= chars.length; index += 1) {
    if (index === chars.length || chars[index] === "\n") {
      ranges.push({ start, end: index });
      start = index + 1;
    }
  }
  if (ranges.length === 0) {
    ranges.push({ start: 0, end: 0 });
  }
  return ranges;
}

function cursorLogicalPosition(
  cursor: number,
  ranges: Array<{ start: number; end: number }>,
): { line: number; col: number } {
  for (let line = 0; line < ranges.length; line += 1) {
    const range = ranges[line];
    if (cursor <= range.end) {
      return { line, col: Math.max(0, cursor - range.start) };
    }
  }
  const last = ranges[ranges.length - 1];
  return { line: ranges.length - 1, col: Math.max(0, last.end - last.start) };
}

function wrappedLinesWithCursor(
  chars: string[],
  cursor: number,
  width: number,
): { lines: string[]; cursorLine: number; cursorCol: number } {
  const lines: string[] = [""];
  let line = 0;
  let colChars = 0;
  let colDisplay = 0;
  let cursorLine = 0;
  let cursorCol = 0;

  for (let index = 0; index <= chars.length; index += 1) {
    if (index === cursor) {
      cursorLine = line;
      cursorCol = colChars;
    }
    if (index === chars.length) {
      break;
    }

    const ch = chars[index];
    if (ch === "\n") {
      line += 1;
      colChars = 0;
      colDisplay = 0;
      lines[line] = "";
      continue;
    }

    const chWidth = charDisplayWidth(ch);
    if (colDisplay > 0 && colDisplay + chWidth > width) {
      line += 1;
      colChars = 0;
      colDisplay = 0;
      lines[line] = "";
    }
    lines[line] += ch;
    colChars += 1;
    colDisplay += chWidth;
    if (colDisplay >= width) {
      line += 1;
      colChars = 0;
      colDisplay = 0;
      lines[line] = "";
    }
  }

  return { lines, cursorLine, cursorCol };
}

function renderCursorLine(text: string, cursorCol: number, visible: boolean): string {
  const chars = Array.from(text);
  const clampedCol = Math.max(0, Math.min(cursorCol, chars.length));
  const before = escapeTags(chars.slice(0, clampedCol).join(""));
  const cursorChar = chars[clampedCol] ?? " ";
  const after = escapeTags(chars.slice(clampedCol + 1).join(""));

  if (visible) {
    return `${before}{black-fg}{white-bg}${escapeTags(cursorChar)}{/white-bg}{/black-fg}${after}`;
  }
  if (clampedCol < chars.length) {
    return `${before}${escapeTags(cursorChar)}${after}`;
  }
  return `${before} `;
}

function buildAgentStatusStrip(state: ChatUiState, width: number): string {
  const contentWidth = Math.max(1, width - 1);

  if (state.agents.length === 0) {
    return "{gray-fg}no agents{/gray-fg}";
  }

  const parts: string[] = [];
  let used = 0;

  for (let index = 0; index < state.agents.length; index += 1) {
    const agent = state.agents[index];
    const dot = renderStatusDot(agent.status);
    const nameColor = colorForAgentIndex(
      typeof agent.colorIdx === "number" ? agent.colorIdx : index,
    );
    const plain = `● ${agent.name}`;
    const plainWidth = strDisplayWidth(plain);
    const sepPlain = parts.length > 0 ? "  " : "";
    const sepWidth = parts.length > 0 ? 2 : 0;
    if (used + sepWidth + plainWidth > contentWidth) {
      const remaining = contentWidth - used - sepWidth;
      if (remaining <= 0) {
        break;
      }
      const clippedName = truncateToDisplayWidth(agent.name, Math.max(1, remaining - 2));
      const clippedPlain = `● ${clippedName}`;
      const clippedWidth = strDisplayWidth(clippedPlain);
      if (used + sepWidth + clippedWidth > contentWidth) {
        break;
      }
      const clippedTagged = `${dot} {${nameColor}-fg}${escapeTags(clippedName)}{/${nameColor}-fg}`;
      parts.push(`${sepPlain}${clippedTagged}`);
      used += sepWidth + clippedWidth;
      break;
    }
    const tagged = `${dot} {${nameColor}-fg}${escapeTags(agent.name)}{/${nameColor}-fg}`;
    parts.push(`${sepPlain}${tagged}`);
    used += sepWidth + plainWidth;
  }

  return parts.join("");
}

export const __uiTestHelpers = {
  wrappedLinesWithCursor,
};
