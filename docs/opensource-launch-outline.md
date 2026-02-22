# CrewForge 开源发布纲要

更新时间：2026-02-22
状态：发布与测试门禁已通过（2026-02-21），进入曝光执行阶段

---

## 一、项目定位（一句话）

**目标：** 让陌生人在 5 秒内判断"这跟我有没有关系"。

**待定。** 完成前需回答：

- 核心名词是什么？orchestrator / runtime / tool / framework？
- 主语落在哪里？多个 agent / opencode / MCP room？
- 与 CrewAI / AutoGen / LangGraph 的本质区别用一句话怎么说？

---

## 二、核心理念（项目灵魂）

> 这部分决定项目有没有差异点，也是 README 和 AI agent websearch 最容易抓取的核心信息。

需要回答以下问题，用自己的语言写下来（不必精炼，原话即可，后期再提炼）：

1. **为什么是 room 模型**，而不是 pipeline / graph / chain？
   - 设计直觉是什么？room 想解决什么别的模型没解决的问题？

2. **为什么 human 是 room 的参与者**，而不只是 prompt 发起者？
   - 这背后的协作哲学是什么？

3. **为什么基于 opencode** 而不是自己实现 LLM client？
   - 信任已有工具链 vs 重复造轮子的权衡是什么？

4. **为什么用 MCP** 作为 agent 间通信协议？
   - 为什么不用自定义 REST / WebSocket？

---

## 三、目标用户画像

**待定。** 候选方向（确认主要用户群体）：

- opencode 重度用户，想跑多 agent 协作？
- 想用 MCP 搭 multi-agent 系统的开发者？
- 想要轻量 CLI 替代 Python 系 agent 框架的 Rust 圈用户？

---

## 四、技术卖点清单

> 用于 README features 区域 + 搜索关键词覆盖。

初步列举，待完善：

- MCP-native：协议级集成，不是 hack
- Rust 单二进制，npm 分发，零运行时依赖
- room 模型：消息共享、历史可持久化、支持 resume
- human 作为 room 成员参与对话，非旁观者
- 不绑定 model / provider，通过 opencode 抽象层接入

---

## 五、对比定位

> 主动说清楚与以下工具的区别，有助于命中 "X vs Y" 类搜索。

| 对比对象 | CrewForge 的差异 |
|----------|-----------------|
| CrewAI / AutoGen | Python 框架，自管 LLM client；CrewForge 是 CLI 工具，复用 opencode，无 Python 依赖 |
| LangGraph | 图结构编排，代码配置；CrewForge 是 room 模型，对话驱动，人类参与其中 |
| opencode 单 agent | 单线程对话；CrewForge 加 room 层让多 agent 并发协作、互相读取彼此输出 |

---

## 六、发布门禁（曝光前必须全部通过）

> 每次发布必须按顺序完成，通过后才能进入曝光渠道。

本轮状态：门禁已于 2026-02-21 全部通过。

- [x] `cargo test` 全部通过
- [x] `Cargo.toml` 版本号已更新
- [x] `git tag vX.Y.Z && git push origin vX.Y.Z` 已执行
- [x] GitHub Actions release workflow 全部 job 成功（build + publish）
- [x] `npm view crewforge version` 返回预期版本号
- [x] `npm i -g crewforge && crewforge --version` 在本地验证可用

---

## 七、曝光渠道计划

> 前置条件：第六节发布门禁全部通过后再执行。

### 第一批（发布当天）

- [x] GitHub repo description + topics 设置
- [x] npm package `keywords` 字段
- [x] README 首屏措辞定稿

### 第二批（发布后）

- [ ] Hacker News：`Show HN: CrewForge – ...`（标题待定，依赖一句话定位）
- [ ] Reddit：r/rust + r/LocalLLaMA
- [ ] opencode 官方社区 / Discord

### 关键词覆盖目标

需在 README 正文自然覆盖以下词：

```
multi-agent  MCP  opencode  agent orchestration
model context protocol  multi-agent chat  LLM agents CLI  Rust CLI
```

---

## 八、待完成事项

1. 填写"一句话定位"
2. 用自己的话回答第二节四个问题（原始素材）
3. 确认目标用户画像主要方向
4. 基于以上，起草 README 首屏（约 200 字）
