# CrewForge TUI Phase C 规格

更新时间：2026-02-22
状态：待实现（在 init/chat v2 完成后执行）

## 1. 目标

将 chat runtime 的 TTY 渲染层从 `rustyline-async` 替换为 `ratatui` + `tui-textarea`，实现：

1. 消息区与输入区物理分离，互不干扰。
2. 消息历史可滚动。
3. 输入框支持多行编辑（`Shift+Enter` 插入换行，`Enter` 提交）。
4. 保持所有业务逻辑、非 TTY 路径、集成测试不变。

## 2. 范围

### 变动文件

| 文件 | 变动类型 |
|------|----------|
| `Cargo.toml` | 新增 4 个依赖 |
| `src/tui.rs` | 新建，约 200 行 |
| `src/chat.rs` | 替换 TTY 分支 + `ChatRuntime` 字段变更 |
| `src/main.rs` | 注册 `mod tui` |

### 不变文件

- 所有 agent / hub / kernel / provider / scheduler 逻辑
- 非 TTY 路径（`BufReader stdin` 分支）
- cliclack preflight（顺序执行，在 ratatui 启动前已完成）
- `tests/` 下所有集成测试（走非 TTY 路径，无需修改）

## 3. 新增依赖

在 `Cargo.toml` `[dependencies]` 中追加：

```toml
ratatui      = "0.29"
tui-textarea = { version = "0.7", features = ["crossterm"] }
crossterm    = { version = "0.28", features = ["event-stream"] }
futures      = "0.3"
```

`crossterm` 已是 `ratatui` 的传递依赖，但需要显式声明以启用 `event-stream` feature（`EventStream` 异步事件流需要它）。

## 4. 布局设计

```
┌──────────────────────────────────────┐
│ [dim] Room started. Session: ...     │
│ [10:30] Rex: 帮我分析一下这个问题    │  ← messages 区
│ [10:31] Codex: 我来拆解一下...      │    填满剩余高度，可上下滚动
│ [10:32] Kimi: 补充一个反例...       │
│                                      │
├──────────────────────────────────────┤
│ └─ Rex>                              │  ← tui-textarea
│                                      │    弹性高度，最少 1 行，最多 5 行
└──────────────────────────────────────┘
```

Layout 使用两个 `Constraint`：

- 消息区：`Constraint::Min(0)`（填满剩余空间）
- 输入区：`Constraint::Max(7)`（边框 2 + 最多 5 行内容）

## 5. 按键映射

| 按键 | 行为 |
|------|------|
| `Enter`（无修饰键） | 提交消息，清空输入框，`auto_scroll = true` |
| `Shift+Enter` | 输入框内插入换行 |
| `PgUp` / `Ctrl+Up` | 消息区向上滚动，`auto_scroll = false` |
| `PgDn` / `Ctrl+Down` | 消息区向下滚动；到底时 `auto_scroll = true` |
| `Ctrl+C` / `Ctrl+D` | 触发退出流程 |
| 其余按键 | 全部交给 `tui-textarea` 处理（光标、删除、undo/redo 等） |

`/help` / `/agents` / `/exit` 等文本命令保持不变，在提交逻辑中继续匹配。

## 6. 自动滚动逻辑

```
auto_scroll = true   （默认，跟随最新消息）
    │
    ├── 收到新 DisplayLine            → 若 auto_scroll，滚到底
    ├── 用户 PgUp / Ctrl+Up           → auto_scroll = false
    ├── 用户 PgDn 且已在底部          → auto_scroll = true
    └── 用户提交消息                  → auto_scroll = true
```

## 7. 数据结构

### 7.1 `DisplayLine`（新建于 `src/tui.rs`）

```rust
pub enum DisplayLine {
    System(String),
    Human {
        ts: String,
        speaker: String,
        text: String,
    },
    Agent {
        ts: String,
        speaker: String,
        text: String,
        agent_idx: usize,   // 用于从 AGENT_COLORS 取色
    },
}
```

渲染时统一转换为 ratatui `Line<'_>`（`Span` + `Style`），不再使用 ANSI 转义字符串。

颜色映射（替换现有 ANSI 常量）：

| 原常量 | ratatui Style |
|--------|---------------|
| `COLOR_DIM` | `Style::new().dim()` |
| `COLOR_HUMAN` (cyan) | `Style::new().fg(Color::Cyan)` |
| `AGENT_COLORS[0]` (green) | `Style::new().fg(Color::Green)` |
| `AGENT_COLORS[1]` (yellow) | `Style::new().fg(Color::Yellow)` |
| `AGENT_COLORS[2]` (magenta) | `Style::new().fg(Color::Magenta)` |
| `AGENT_COLORS[3]` (blue) | `Style::new().fg(Color::Blue)` |
| `AGENT_COLORS[4]` (red) | `Style::new().fg(Color::Red)` |

### 7.2 `TerminalGuard`（新建于 `src/tui.rs`）

```rust
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            std::io::stdout(),
            crossterm::terminal::LeaveAlternateScreen
        );
    }
}
```

在进入 ratatui loop 前构造，离开作用域（包括 panic）时自动还原终端状态。

## 8. `ChatRuntime` 字段变更

```rust
// 删除
tty_writer: Arc<StdMutex<Option<SharedWriter>>>,

// 新增
msg_tx: tokio::sync::mpsc::UnboundedSender<DisplayLine>,
```

同时删除：
- `attach_tty_writer()`
- `detach_tty_writer()`

`render_room_line` 改为：

```rust
fn send_display_line(&self, line: DisplayLine) {
    // TTY 时 channel 有效；非 TTY 时对端已 drop，send 返回 Err，
    // 调用处 fallback 到 println!（见下方非 TTY 说明）
    let _ = self.msg_tx.send(line);
}
```

非 TTY 路径中，`msg_tx` 对端（`msg_rx`）在构造时直接 drop，所有 send 静默失败，调用处使用独立的 `println!` fallback，与现有行为一致。

## 9. `run_chat` TTY 分支替换

原有 `if is_tty { rustyline-async loop }` 整块替换为：

```rust
if is_tty {
    let (msg_tx, msg_rx) = tokio::sync::mpsc::unbounded_channel::<DisplayLine>();
    // 将 msg_tx 注入 runtime（runtime 需在此之前构造时接收）

    tui::run_tui_loop(runtime, msg_rx, stop_flag).await?;
} else {
    // 非 TTY 路径不变
}
```

## 10. `tui::run_tui_loop` 函数签名

```rust
pub async fn run_tui_loop(
    runtime: Arc<ChatRuntime>,
    mut msg_rx: tokio::sync::mpsc::UnboundedReceiver<DisplayLine>,
    stop_flag: Arc<AtomicBool>,
) -> Result<()>
```

内部结构：

```
1. 构造 TerminalGuard
2. enable_raw_mode + EnterAlternateScreen
3. 创建 CrosstermBackend + Terminal
4. 创建 TextArea，配置：
   - 无边框标题以外保持默认
   - placeholder 文字（可选）
5. 初始化 display_lines: Vec<DisplayLine>，scroll_offset: u16，auto_scroll: bool = true
6. 创建 crossterm::event::EventStream
7. 主循环 loop:
   a. drain msg_rx（try_recv 直到 Empty）
   b. terminal.draw(render)
   c. tokio::select!:
      - event_stream.next() → 处理键盘 / resize
      - msg_rx.recv()       → 追加消息
   d. 检查 stop_flag
```

## 11. 渲染函数

```rust
fn render(
    frame: &mut Frame,
    lines: &[DisplayLine],
    textarea: &TextArea,
    scroll_offset: u16,
)
```

- 使用 `Layout::vertical([Constraint::Min(0), Constraint::Max(7)])` 分割
- 消息区：`Paragraph::new(lines_to_text(lines)).scroll((scroll_offset, 0))` + `Block::bordered()`
- 输入区：直接 `frame.render_widget(textarea, chunks[1])`

## 12. 终端恢复顺序

```
stop_flag = true
→ 退出 select! 主循环
→ wait_active_tasks(1000ms)
→ mcp_server.stop()
→ TerminalGuard drop（disable_raw_mode + LeaveAlternateScreen）
→ 打印 resume hint（此时终端已恢复正常模式，println 安全）
```

## 13. 测试验收要点

1. TTY 路径：`Shift+Enter` 可在输入框内插入换行，`Enter` 提交完整多行内容。
2. 消息区在 agent 回复到达时自动滚到底部。
3. 用户 PgUp 后消息区停止自动跟随，PgDn 到底后恢复。
4. `Ctrl+C` 正常退出，终端恢复到正常模式（无残留 raw mode）。
5. panic 时 `TerminalGuard` 正确还原终端。
6. 非 TTY 路径（`--dry-run` 集成测试）行为不变。
7. cliclack preflight 结束后 ratatui loop 正常启动，无终端状态冲突。
