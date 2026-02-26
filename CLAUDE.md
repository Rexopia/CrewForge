# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Repository Layout

Two sub-projects, both required to ship the `crewforge` binary:

| Path | Language | Role |
|------|----------|------|
| `crewforge-rs/` | Rust | Core runtime: session kernel, MCP hub/server, scheduler, TUI, provider stack, auth, agent loop |
| `crewforge-ts/` | Node/TypeScript | npm package launcher: resolves platform-specific Rust binary, forwards argv/signals |
| `vendor/zeroclaw/` | Rust | Upstream source used as porting reference only — do **not** modify |

The TypeScript wrapper resolves the Rust binary from optional npm packages (`@crewforge/core-<platform>`), or falls back to `crewforge-rs/target/debug/crewforge` for local development.

## Commands

### Rust core (`crewforge-rs/`)

```bash
# Build
cargo build --manifest-path crewforge-rs/Cargo.toml

# Run all tests
cargo test --manifest-path crewforge-rs/Cargo.toml

# Run a single test by name
cargo test --manifest-path crewforge-rs/Cargo.toml <test_name>

# Run tests in a specific module
cargo test --manifest-path crewforge-rs/Cargo.toml --lib auth::

# Lint + format check
cargo fmt --manifest-path crewforge-rs/Cargo.toml --all -- --check
cargo clippy --manifest-path crewforge-rs/Cargo.toml --all-targets -- -D warnings

# Run directly from source (after build)
./crewforge-rs/target/debug/crewforge --help
```

### TypeScript launcher (`crewforge-ts/`)

```bash
npm run build --prefix crewforge-ts   # compile TS → dist/
npm test --prefix crewforge-ts        # build + run node:test suite
```

## Architecture

### `crewforge chat` runtime (multi-agent room)

```
User → chat.rs → SessionKernel (kernel.rs, JSONL on disk)
                      └→ RoomHub (hub.rs, rate-limiting, wake budget)
                              └→ RoomHubMcpServer (mcp_server.rs, axum + rmcp)
                                      ↑ MCP tools: hub_get_unread / hub_ack / hub_post
                              └→ OpencodeCliProvider (opencode_provider.rs)
                                      ↑ spawns `opencode` subprocess per agent turn
```

**Key constraint:** `crewforge chat` agents run via `opencode` subprocess — they do **not** use the Rust provider stack (`src/provider/`). The Rust provider stack is used only by `crewforge agent`.

### `crewforge agent` (native Rust agent REPL)

```
agent_cmd.rs → AgentSession (agent/loop_.rs)
                    └→ dispatcher.rs  (tool dispatch)
                    └→ history.rs     (conversation memory)
                    └→ provider::create_provider()  (Rust provider stack)
```

### Library crate (`crewforge` lib — `src/lib.rs`)

Exports four public modules consumed by other crates/tools:

- `crewforge::agent` — `AgentSession`, `AgentSessionConfig`, `Tool` trait, events
- `crewforge::auth` — `AuthService`, `default_state_dir()`, OAuth flows
- `crewforge::provider` — `create_provider()`, `Provider` trait, `ProviderRuntimeOptions`
- `crewforge::security` — `SecretStore` (ChaCha20-Poly1305)

### Provider stack (`src/provider/`)

| File | Purpose |
|------|---------|
| `traits.rs` | `Provider` trait, `ChatMessage`, `ToolSpec`, etc. |
| `compatible.rs` | `OpenAiCompatibleProvider` base for most APIs |
| `reliable.rs` | Retry wrapper (`ReliableProvider`) |
| `router.rs` | Round-robin `RouterProvider` |
| `mod.rs` | `create_provider()` factory, `ProviderRuntimeOptions` |

Factory aliases: `"anthropic"/"claude"`, `"openai"/"gpt"`, `"gemini"/"google"`, `"ollama"`, `"openrouter"`, `"glm"/"zhipuai"`, `"moonshot"/"kimi"`, `"qwen"`, `"minimax"`, `"deepseek"`, `"groq"`, `"mistral"`, `"xai"`, `"copilot"/"github-copilot"`, `"openai-codex"/"codex"`.

### Auth system (`src/auth/`)

Persistent profiles at `~/.crewforge/auth-profiles.json`. Optional ChaCha20 encryption via `CREWFORGE_SECRETS_ENCRYPT=1`.

Token resolution priority in `crewforge agent`: `--api-key` flag → env var (e.g. `ANTHROPIC_API_KEY`) → auth profile lookup.

## CLI Commands

```
crewforge init       # manage global agent profiles (~/.crewforge/profiles.json)
crewforge chat       # start multi-agent room (uses opencode subprocess)
crewforge auth       # manage provider credentials (OAuth / API keys)
crewforge agent      # interactive single-agent REPL (native Rust provider stack)
```

`crewforge auth` subcommands: `login`, `paste-redirect`, `paste-token`, `refresh`, `logout`, `use`, `list`, `status`.

## Key Patterns and Gotchas

### Binary vs library imports
Modules declared in `main.rs` (e.g. `auth_cmd.rs`, `agent_cmd.rs`) are part of the **binary** crate. They must use `crewforge::` prefix to access library items — **not** `crate::`:

```rust
// Correct in auth_cmd.rs / agent_cmd.rs:
use crewforge::auth::{AuthService, default_state_dir};

// Wrong — crate:: refers to the binary, not the library:
use crate::auth::AuthService;
```

### Rust 1.93.1 env var safety
`std::env::set_var` / `remove_var` require `unsafe {}` in test code. Add a safety comment:

```rust
unsafe {
    // Safety: tests run single-threaded (cfg(test)); no concurrent env access
    std::env::set_var("KEY", "value");
}
```

### Session storage
- Global profiles: `~/.crewforge/profiles.json`
- Auth profiles: `~/.crewforge/auth-profiles.json`
- Pending OAuth state: `~/.crewforge/auth-{provider}-pending.json`
- Room sessions: `.room/sessions/session-<id>.jsonl` (per-project)
- Room config: `.room/room.json` (per-project)

### Adding a provider
1. Implement `Provider` trait in `src/provider/<name>.rs`
2. Add `pub mod <name>;` in `src/provider/mod.rs`
3. Add match arm to `create_provider()` factory
4. Add env var name to `default_api_key_env()` if applicable

## Release Process

Tag must exactly match `crewforge-rs/Cargo.toml` version (`vX.Y.Z` ↔ `X.Y.Z`). Do not force-move a tag on mismatch — bump to next patch and create a new tag instead. Do not manually edit `crewforge-ts/package.json` version; the release workflow syncs it from the tag.

```bash
# 1. validate
cargo clippy --manifest-path crewforge-rs/Cargo.toml --all-targets
cargo test --manifest-path crewforge-rs/Cargo.toml
npm test --prefix crewforge-ts
cargo build --release --manifest-path crewforge-rs/Cargo.toml

# 2. smoke test launcher + core
CREWFORGE_CORE_BIN="$(pwd)/crewforge-rs/target/release/crewforge" \
  node crewforge-ts/dist/bin/crewforge.js --version
CREWFORGE_CORE_BIN="$(pwd)/crewforge-rs/target/release/crewforge" \
  node crewforge-ts/dist/bin/crewforge.js chat --dry-run

# 3. verify tag matches Cargo version (hard gate)
REL_TAG="vX.Y.Z"
CARGO_VERSION="$(awk -F '"' '/^version = / {print $2; exit}' crewforge-rs/Cargo.toml)"
[ "$REL_TAG" = "v$CARGO_VERSION" ] || { echo "blocked: tag=$REL_TAG cargo=v$CARGO_VERSION"; exit 1; }

# 4. push tag to trigger GitHub Actions release workflow
git tag vX.Y.Z
git push origin vX.Y.Z
```

Target platforms: `linux-x64`, `linux-arm64`, `darwin-x64`, `darwin-arm64` (Windows disabled).
