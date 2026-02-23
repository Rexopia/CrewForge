# Contributing to CrewForge

## Reporting Issues

**Bug reports** — please include:
- OS and architecture (e.g. macOS arm64, Linux x64)
- `crewforge --version` output
- Steps to reproduce and what you expected vs. what happened

**Feature requests** — describe the use case you have in mind. No need to have a solution ready.

Open issues at: https://github.com/Rexopia/crewforge/issues

## Development Setup

**Prerequisites:**

| Tool | Recommended version | Notes |
|------|---------------------|-------|
| Rust | 1.93 | Recommended; install via [rustup](https://rustup.rs) |
| Node.js | 24 | Recommended; for npm distribution layer and running the glue script |
| opencode | 1.2.9 | `npm i -g opencode-ai` — needed to run agents locally |

**Build and test:**

```bash
git clone https://github.com/Rexopia/crewforge.git
cd crewforge

cargo build          # debug build
cargo test           # run all tests
cargo build --release  # release build (the actual binary)
```

**Test the npm glue layer locally:**

```bash
cargo build --release
# point the glue script directly at the local binary
node -e "
  process.env.npm_config_prefix = '.';
  " bin/crewforge.js --version
# or just run the binary directly:
./target/release/crewforge --help
```

## Pull Requests

- For non-trivial changes, open an issue first to discuss the direction
- Make sure `cargo test` passes before submitting
- Commit messages follow [Conventional Commits](https://www.conventionalcommits.org/):
  `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `perf:`, `test:`, `build:`, `ci:`, `style:`, `revert:`
- Install local hooks once so invalid commit messages are blocked before commit:
  `./scripts/install-git-hooks.sh`
- Keep PRs focused — one concern per PR
