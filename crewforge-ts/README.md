# crewforge

TypeScript/Node frontend package for CrewForge.

- Resolves and launches the platform-specific CrewForge core binary package.
- Acts as a thin launcher only (`spawn + stdio: inherit`).
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
