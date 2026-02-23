# crewforge

TypeScript/Node frontend package for CrewForge.

- Resolves and launches the platform-specific CrewForge core binary package.
- Runs the interactive chat TUI (blessed) and talks to Rust core via JSONL RPC.
- Supports local development fallback to `../crewforge-rs/target/debug/crewforge`.

Install:

```bash
npm i -g crewforge
```

Local development:

```bash
cargo build --manifest-path ../crewforge-rs/Cargo.toml
npm run build
node dist/bin/crewforge.js --help
```
