# CrewForge 纯 Rust 一次性重构方案（Big-Bang）

更新时间：2026-02-21  
适用仓库：`/root/workspace/CrewForge`

## 1. 决策摘要

本项目采用 **纯 Rust + 单二进制** 的一次性重构方案，不保留长期 Rust+TS 双栈。

核心决策：
- 运行时统一为 Rust：`kernel` / `hub` / `mcp_server` / `scheduler` / `provider_opencode` / `tui`
- MCP Hub 采用 **server 视角实现**，直接基于 `rmcp` 的 HTTP server transport
- 重构采用“并行开发 + 一次切换”模式：旧实现只作为回归基线，不做长期双写

推荐难度评估：`6.5/10`（中等偏上，可控）

## 2. 目标与非目标

### 2.1 目标

1. 保持当前产品语义不变：
- discussion-first（不强制收敛）
- agent 自愿发言
- `[DROP]` / `[SKIP]` 为合法 no-publish

2. 保持当前运行哲学：
- `Room Kernel` 是 transcript 单一事实源
- 仅 `event_loop` 调度
- provider 仅 `opencode`
- watchdog 在首条 human 消息后启动
- 跨 agent 并发唤醒；agent 内部串行（`running`/`dirty`）

3. 完整实现 MCP Hub 工具语义：
- `hub_get_unread`
- `hub_ack`
- `hub_post`

4. 保持可运维性：
- 会话持久化到 `.room/sessions/*.jsonl`
- 异常可观测，可恢复

### 2.2 非目标

1. 不做 `parallel_sync` 模式
2. 不做 provider 多分支（`codex_cli` / `kimi_cli` / `claude_cli`）
3. 不新增用户命令集（继续 `/help` `/agents` `/exit`）
4. 不做长期 TS 适配层

### 2.3 CLI 统一约定（强约束）

从本次重构开始，**所有用户侧 CLI 命令统一使用 `crewforge` 前缀**，不再以 `npm run ...` 作为主入口。

最简对齐命令：
1. `crewforge chat`

命令迁移对照：
1. 旧：`npm run chat` -> 新：`crewforge chat`

说明：
1. `crewforge chat` 是替代 `npm run chat` 的标准入口。
2. 文档、脚本、测试说明、CI 示例中出现聊天启动命令时，统一写 `crewforge chat`。
3. 如需保留 `npm run chat`，仅作为过渡别名，不作为规范命令写入新文档。

## 3. 架构总览（目标态）

## 3.1 仓库结构

```text
CrewForge/
  Cargo.toml
  Cargo.lock
  crates/
    brainstorm-app/            # CLI 入口 + 配置加载 + 进程生命周期
    brainstorm-kernel/         # transcript、event、session storage
    brainstorm-hub/            # unread/ack/post 语义与 rate limit
    brainstorm-mcp-server/     # rmcp server + streamable http transport
    brainstorm-provider-opencode/ # opencode 子进程封装 + json 流解析
    brainstorm-scheduler/      # event loop + watchdog + worker state
    brainstorm-tui/            # ratatui UI、输入、日志渲染、命令处理
    brainstorm-types/          # 共享模型（RoomEvent/ToolPayload/...）
  tests/
    replay/                    # transcript golden tests
    integration/               # mcp/http/scheduler/provider 集成测试
  docs/
    pure-rust-rewrite-plan.md
```

## 3.2 运行时边界

1. `brainstorm-app` 负责启动顺序：
- load config
- init kernel/session
- start mcp server
- start scheduler
- start tui

2. `brainstorm-kernel` 只负责“事实写入/读取”：
- append event（单调递增 seq）
- query unread by agent
- ack watermark 更新

3. `brainstorm-hub` 承载业务规则：
- unread 过滤策略
- ack 合法性校验
- rate limit（每 agent 时间窗）

4. `brainstorm-mcp-server` 只做协议适配：
- tool registration
- request decode/encode
- auth/token check（如启用）

5. `brainstorm-scheduler` 只做路由与调度：
- human 输入事件 -> fanout
- gather tick -> candidate agent wake
- 每 agent worker 内串行处理

## 4. MCP 实现路径（关键）

## 4.1 依赖与 feature

`brainstorm-mcp-server/Cargo.toml`：

```toml
[dependencies]
rmcp = { version = "0.15.0", default-features = false, features = [
  "macros",
  "server",
  "transport-streamable-http-server",
] }
tokio = { version = "1", features = ["rt-multi-thread", "macros", "sync", "time"] }
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tracing = "0.1"
anyhow = "1"
```

说明：
- `transport-streamable-http-server` 是当前 server 侧 HTTP transport 的关键路径
- 避免手写协议层 HTTP transport，减少偏差和维护负担

## 4.2 Tool 契约

工具名固定：
1. `hub_get_unread`
2. `hub_ack`
3. `hub_post`

入参与返回建议统一：
- 入参：显式 `agent_id`、`session_id`、可选 `cursor`/`limit`
- 返回：统一 envelope `{ ok, data, error, seq }`

错误分级：
- `BAD_REQUEST`（参数不合法）
- `UNAUTHORIZED`（token/session 不匹配）
- `RATE_LIMITED`
- `INTERNAL`

## 4.3 Server Handler 设计

建议通过 `ServerHandler`（或等价 handler trait）封装：
- 工具注册在启动时完成，不在请求路径动态变更
- handler 内不持有可变全局状态，改为：
  - `Arc<HubService>`
  - `Arc<KernelStore>`
  - `Arc<RateLimiter>`

## 5. 并发与状态机

## 5.1 全局事件流

事件类型：
1. `HumanSubmitted`
2. `WatchdogTick`
3. `AgentWakeRequested`
4. `AgentResponseReceived`
5. `ToolPostAccepted`
6. `ToolAckAccepted`
7. `SystemError`

单线程事件编排（逻辑单线程，底层可多线程执行 IO）：
- 事件序列在 scheduler 中串行决策
- 外部 IO（provider/MCP）异步并发

## 5.2 Per-Agent Worker

每个 agent 状态：
- `running: bool`
- `dirty: bool`
- `last_post_at: Instant`
- `posts_in_window: u32`

规则：
1. 收到 wake：
- 若 `running=true` -> `dirty=true`，不并发重入
- 若 `running=false` -> 启动 worker
2. worker 结束：
- 若 `dirty=true` -> 清 `dirty` 并立即再跑一轮
- 否则退出并置 `running=false`

## 5.3 数据一致性

`kernel` 内部保证：
- append 原子化（同一 session 内严格 seq 递增）
- ack watermark 单调不回退
- post 与 ack 写入失败可重试且幂等

## 6. Provider（opencode）重构要点

## 6.1 子进程调用

命令形态维持：
- `opencode run --format json --dir <workspace-dir> --agent brainstorm-room -m <model> <prompt>`
- continuation 用 `-s <sessionID>`
- 环境变量 `OPENCODE_CONFIG_DIR=<agent-runtime-dir>`

## 6.2 流式解析

实现要求：
1. 容忍 chunk 边界不齐整（按行缓冲 + JSON 解析）
2. 未知字段忽略，不因扩展字段失败
3. 非法事件转结构化 warning，不直接 panic

## 6.3 超时与取消

每次 agent 轮次配置：
- `spawn_timeout_ms`
- `read_timeout_ms`
- `hard_kill_timeout_ms`

超时策略：
- soft cancel -> hard kill
- 记录原因到 transcript system event

## 7. TUI 方案（纯 Rust）

框架建议：
- `ratatui` + `crossterm`

UI 目标：
1. 保持底部输入区语义：
- `┌─ Input`
- `└─ Rex>`
2. 发送后输入框清空，transcript 保留规范化回显
3. 命令支持：
- `/help`
- `/agents`
- `/exit`

工程要求：
- UI 与 runtime 解耦（channel 驱动）
- UI 不直接读写 kernel/hub

## 8. 一次性重构执行剧本（可直接开工）

## 8.1 Phase 0：规格冻结（1-2 天）

输出物：
1. `docs/contracts/mcp-tools.md`
2. `docs/contracts/transcript-schema.md`
3. `docs/contracts/scheduler-semantics.md`

Gate：
- 旧实现行为快照完成（抽样会话 + JSONL 样本）

## 8.2 Phase 1：骨架可运行（4-6 天）

目标：
- 单二进制可启动
- MCP server 可处理 3 个 hub 工具最小路径
- dry-run 可跑通事件循环

输出物：
1. workspace crate 脚手架
2. `kernel` append/query 最小实现
3. `mcp_server` + `hub` 最小实现
4. `scheduler` 最小事件循环

Gate：
- `cargo test --workspace` 通过
- 最小 E2E: “人类输入 -> agent 被唤醒 -> post 入 transcript”

## 8.3 Phase 2：功能对齐（7-10 天）

目标：
- provider、rate limit、ack 语义、命令、会话持久化全部对齐

输出物：
1. `provider_opencode` 完整实现
2. per-agent rate limit
3. worker `running/dirty` 行为对齐
4. `.room/sessions/*.jsonl` 持久化与恢复

Gate：
- 合同测试 + 集成测试全绿
- replay 对比达到语义等价

## 8.4 Phase 3：稳定化（2-3 周）

目标：
- 并发、异常恢复、长会话稳定

输出物：
1. replay golden 测试集
2. 压测脚本（多 agent + 高频 tick）
3. 故障注入测试（provider 超时/坏 JSON/网络中断）

Gate：
- 长跑测试无崩溃/无死锁
- 无消息丢失/重复 ack

## 8.5 Phase 4：切换与收尾（3-5 天）

目标：
- 一次切换到 Rust 主实现

执行：
1. 将 Rust 二进制设为默认启动入口
2. 旧实现保留只读基线，不再接新开发
3. 发布首个稳定 tag

Gate：
- 验收清单全部通过（见第 11 节）

## 9. 测试矩阵（必须）

## 9.1 单元测试

覆盖：
1. unread 计算
2. ack watermark
3. rate limit 窗口重置
4. scheduler 状态转移

## 9.2 契约测试

覆盖：
1. `hub_get_unread` 入参/返回
2. `hub_ack` 幂等与单调性
3. `hub_post` 校验与错误码

## 9.3 集成测试

覆盖：
1. MCP HTTP server 端到端
2. provider 子进程真实样本解析
3. TUI 命令到 runtime 事件映射

## 9.4 Replay 测试

来源：
- 旧实现 `.room/sessions/*.jsonl` 样本

判定：
- 允许文本细节差异
- 不允许语义差异（事件顺序约束、ack/post 行为、rate limit 决策）

## 10. CI/CD 与工程规范

最小门禁：
1. `cargo fmt --check`
2. `cargo clippy --workspace --all-targets -- -D warnings`
3. `cargo test --workspace`
4. replay smoke tests

建议：
- nightly job 执行长跑与故障注入
- release profile 打开 LTO + strip

## 11. 验收清单（切换前必须全满足）

1. MCP 契约：`hub_get_unread/hub_ack/hub_post` 全部通过契约测试  
2. 并发正确性：多 agent 场景无丢消息、无重复 ack、无 worker 重入竞态  
3. 运行稳定性：长会话压测通过，异常可恢复  
4. 行为一致性：replay 语义等价  
5. 可运维性：日志、错误码、超时原因可追踪  
6. 可发布性：CI 全绿，可生成单二进制发布物

## 12. 风险与回滚

主要风险：
1. tool 契约与旧语义偏移
2. provider 流解析异常导致事件缺失
3. 调度状态机边界 bug

缓解：
1. 先写契约测试再实现
2. provider 样本库回归
3. 故障注入 + 压测前置

回滚策略（一次切换失败时）：
1. 保留旧实现可独立启动入口（只回滚入口，不回滚代码历史）
2. 新实现产生的数据写入隔离目录，避免污染旧会话
3. 回滚后仅修复阻断问题，再次通过同一验收清单后重切

## 13. 开工顺序（今天可执行）

1. 初始化 workspace 与 8 个 crates  
2. 先落 `brainstorm-types` + `brainstorm-kernel`  
3. 落 `brainstorm-hub` + `brainstorm-mcp-server`（`rmcp` server feature）  
4. 落 `brainstorm-scheduler`（含 worker state）  
5. 落 `brainstorm-provider-opencode`  
6. 落 `brainstorm-tui`  
7. 最后补 replay + integration tests，并开始稳定化

---

该文档是 `../CrewForge` 的执行基线。后续所有重构开发均在该路径进行，不再以 `brainStorm` 仓库作为实现载体。
