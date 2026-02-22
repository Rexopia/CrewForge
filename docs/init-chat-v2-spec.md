# CrewForge Init/Chat V2 规格

更新时间：2026-02-21
状态：已确认，待实现

## 1. 目标

本轮只重构 `init/chat` 引导流程，不做大规模 TUI 视觉升级。

核心目标：
1. `init` 管理全局可用角色（profile）。
2. `chat` 管理当前目录启用哪些角色并启动会话。
3. `--resume` 严格恢复历史会话，不插入重新配置引导。

## 2. 范围与约束

1. 维持当前开发约束：单 room / 单 chat / 多 agents。
2. 保持 `.room/sessions/*.jsonl` 会话日志模型。
3. 本轮不处理 npm 分发层，不处理多 room/多 chat 设计。

## 3. 文件布局

1. 全局配置：`~/.crewforge/profiles.json`
2. 房间配置：`.room/room.json`
3. 角色配置：`.room/agents/<id>/opencode.json`
4. 会话日志：`.room/sessions/<session-id>.jsonl`
5. 会话侧车：`.room/sessions/<session-id>.meta.json`

不新增 `.room/config/`。

## 4. 全局 Profile 结构

`~/.crewforge/profiles.json` 结构如下：

```json
{
  "profiles": [
    {
      "name": "Codex",
      "model": "openai/gpt-5.3-codex",
      "preference": null
    },
    {
      "name": "Kimi",
      "model": "kimi-for-coding/kimi-k2-thinking",
      "preference": "你偏重推理与反例检查"
    }
  ]
}
```

规则：
1. 不包含 `version` 字段。
2. `name` 全局唯一。
3. `name-model-preference` 绑定后不可编辑，只支持删除后重建。

## 5. Name 归一化与唯一性

使用现有 `to_agent_id` 规则做归一化唯一校验，避免路径冲突：
1. 小写化。
2. 非字母数字转为 `-`。
3. 连续分隔符合并。
4. 去除首尾 `-`。
5. 归一化后冲突即报错。

示例：`A B` 与 `A-B` 视为冲突，不允许共存。

## 6. `crewforge init` 语义

### 6.1 默认 `crewforge init`

用途：仅新增全局 profile（交互流程）。

流程：
1. 调用 `opencode models` 获取模型列表。
2. 解析为纯文本逐行列表（每行 `provider/model-id`）。
3. 提供搜索 + 上下选择模型。
4. 输入 `name`（唯一校验 + 归一化冲突校验）。
5. 输入 `preference`（可空）。
6. 确认写入 `~/.crewforge/profiles.json`。

约束：
1. 不允许手工输入 model。
2. `opencode models` 失败则直接失败并提示。

### 6.2 `crewforge init --delete <name>`

用途：删除全局 profile（非交互）。

规则：
1. 仅删除 `~/.crewforge/profiles.json` 对应项。
2. 不清理任何 `.room/agents/*` 历史目录。
3. 找不到 `name` 时返回非 0 并报错。

## 7. Preference 注入规则

`preference` 写入 profile 后，在生成 managed prompt 时按以下规则处理：
1. `preference` 为空：不追加额外 prompt。
2. `preference` 非空：在现有基础 prompt 上追加一层偏好描述。

## 8. `crewforge chat`（不带 `--resume`）

### 8.1 首次使用（无 `.room/room.json`）

流程：
1. 进入引导并询问 `human`（默认 `Rex`）。
2. 从全局 profiles 展示可选角色，默认全部未选。
3. 用户选择本次启用的 `name` 集合。
4. 生成或重写 `.room/room.json`（仅包含本次启用集合）。
5. 对每个启用角色：
   1. 若 `.room/agents/<id>/opencode.json` 已存在则复用（skip）。
   2. 若不存在则初始化补齐。
6. 进入 chat 主循环。

### 8.2 已有房间配置（存在 `.room/room.json`）

先给二选一：
1. 继续当前目录配置：直接进入 chat。
2. 重新配置：重新选择启用角色并重写 `.room/room.json`。

重新配置规则：
1. `.room/agents/` 下历史角色目录全部保留。
2. 仅更新 `.room/room.json` 的当前启用集合。

说明：`roomName` 不在引导中询问，保持内部默认值即可。

## 9. `crewforge chat --resume <session-id|path>`

`--resume` 为严格恢复模式：
1. 直接恢复，不进入“继续/重配”引导。
2. 必须同时存在会话日志与侧车：
   1. `.jsonl`
   2. `.meta.json`
3. 任一缺失则直接失败，不做 fallback。

`meta` 结构：

```json
{
  "human": "Rex",
  "enabledNames": ["Codex", "Kimi"]
}
```

## 10. Resume 遇到已删除 Profile

当 `meta.enabledNames` 中某个 name 已从全局 profiles 删除时：
1. 该角色禁用。
2. 打印 warning。
3. 其他可用角色继续恢复。
4. 若最终可用角色数为 0，则恢复失败并退出。

## 11. 测试验收要点

建议在现有集成测试文件扩展：
1. `tests/init_command.rs`
2. `tests/chat_dry_run.rs`

必测场景：
1. `opencode models` 逐行解析与失败路径。
2. `name` 归一化冲突校验。
3. `init --delete <name>` 成功与 name 不存在失败路径。
4. 首次 chat 默认不选角色行为。
5. 重新配置只改 `room.json`，不删 `.room/agents/*`。
6. `--resume` 跳过引导。
7. 缺少 `.meta.json` 时 resume 失败。
8. resume 时 profile 被删除后的 warning + 禁用逻辑。
9. 全部禁用时 resume 失败。
