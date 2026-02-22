# CrewForge npm 分发层规格

更新时间：2026-02-22
状态：待实现（在 init/chat v2 完成后执行）

## 1. 目标

将 Rust 构建产物通过 npm 分发，使用户可以通过以下方式安装：

```bash
npm i -g crewforge
```

安装后直接使用 `crewforge init` / `crewforge chat` 等命令。

## 2. 整体结构

```
crewforge/                          ← git 仓库根目录
├── Cargo.toml                      ← Rust 包（已有）
├── src/                            ← Rust 源码（已有）
├── package.json                    ← 主 npm 包（新增）
├── bin/
│   └── crewforge.js                ← JS 粘合层入口（新增）
└── npm/                            ← 各平台子包目录（新增）
    ├── crewforge-linux-x64/
    │   └── package.json
    ├── crewforge-linux-arm64/
    │   └── package.json
    ├── crewforge-darwin-x64/
    │   └── package.json
    ├── crewforge-darwin-arm64/
    │   └── package.json
    └── crewforge-win32-x64/
        └── package.json
```

平台子包目录在开发阶段不包含二进制文件，二进制由 CI 构建后注入。

## 3. 支持的平台

| npm 包名                       | os      | cpu   | Rust target triple                  |
|-------------------------------|---------|-------|-------------------------------------|
| `@crewforge/linux-x64`        | linux   | x64   | `x86_64-unknown-linux-gnu`          |
| `@crewforge/linux-arm64`      | linux   | arm64 | `aarch64-unknown-linux-gnu`         |
| `@crewforge/darwin-x64`       | darwin  | x64   | `x86_64-apple-darwin`               |
| `@crewforge/darwin-arm64`     | darwin  | arm64 | `aarch64-apple-darwin`              |
| `@crewforge/win32-x64`        | win32   | x64   | `x86_64-pc-windows-msvc`            |

## 4. 文件内容

### 4.1 主包 `package.json`（根目录）

```json
{
  "name": "crewforge",
  "version": "0.1.0",
  "description": "CrewForge CLI — multi-agent chat orchestrator",
  "bin": {
    "crewforge": "bin/crewforge.js"
  },
  "files": [
    "bin/"
  ],
  "optionalDependencies": {
    "@crewforge/linux-x64":    "0.1.0",
    "@crewforge/linux-arm64":  "0.1.0",
    "@crewforge/darwin-x64":   "0.1.0",
    "@crewforge/darwin-arm64": "0.1.0",
    "@crewforge/win32-x64":    "0.1.0"
  },
  "engines": {
    "node": ">=18"
  }
}
```

> **注意**：`optionalDependencies` 中的版本号仅作为初始占位，不需要手动维护。
> 发布时 `release.yml` 的 publish 步骤会通过 `node -e` 脚本在发布前将所有子包版本号
> 动态更新为当前 tag 版本，确保主包始终依赖同版本的子包。

### 4.2 平台子包 `package.json`（以 linux-x64 为例）

路径：`npm/crewforge-linux-x64/package.json`

```json
{
  "name": "@crewforge/linux-x64",
  "version": "0.1.0",
  "description": "CrewForge binary for linux-x64",
  "os": ["linux"],
  "cpu": ["x64"],
  "files": [
    "crewforge"
  ]
}
```

其余四个平台子包结构相同，替换 `name`、`os`、`cpu`、`files`（Windows 为 `crewforge.exe`）。

### 4.3 JS 粘合层 `bin/crewforge.js`

```js
#!/usr/bin/env node
'use strict';

const { execFileSync } = require('child_process');

const PACKAGES = {
  'linux-x64':    '@crewforge/linux-x64',
  'linux-arm64':  '@crewforge/linux-arm64',
  'darwin-x64':   '@crewforge/darwin-x64',
  'darwin-arm64': '@crewforge/darwin-arm64',
  'win32-x64':    '@crewforge/win32-x64',
};

const key = `${process.platform}-${process.arch}`;
const pkg = PACKAGES[key];

if (!pkg) {
  process.stderr.write(`[crewforge] Unsupported platform: ${key}\n`);
  process.exit(1);
}

const binName = process.platform === 'win32' ? 'crewforge.exe' : 'crewforge';

let binaryPath;
try {
  binaryPath = require.resolve(`${pkg}/${binName}`);
} catch {
  process.stderr.write(
    `[crewforge] Platform binary not found (${pkg}).\n` +
    `Try reinstalling: npm i -g crewforge\n`
  );
  process.exit(1);
}

try {
  execFileSync(binaryPath, process.argv.slice(2), { stdio: 'inherit' });
} catch (e) {
  process.exit(typeof e.status === 'number' ? e.status : 1);
}
```

## 5. GitHub Actions Workflow

### 5.1 `ci.yml`（每次 push/PR，不发布）

路径：`.github/workflows/ci.yml`

```yaml
name: CI

on:
  push:
    branches: [main]
  pull_request:
    branches: [main]

jobs:
  test:
    runs-on: ubuntu-latest
    steps:
      - uses: actions/checkout@v4
      - uses: dtolnay/rust-toolchain@stable
      - uses: Swatinem/rust-cache@v2
      - run: cargo test
```

### 5.2 `release.yml`（只在 `v*` tag 时触发，构建 + 发布）

路径：`.github/workflows/release.yml`

```yaml
name: Release

on:
  push:
    tags:
      - 'v*'

jobs:
  build:
    name: Build ${{ matrix.target }}
    runs-on: ${{ matrix.os }}
    strategy:
      matrix:
        include:
          - os: ubuntu-latest
            target: x86_64-unknown-linux-gnu
            npm_pkg: linux-x64
            bin: crewforge
          - os: ubuntu-latest
            target: aarch64-unknown-linux-gnu
            npm_pkg: linux-arm64
            bin: crewforge
            cross: true
          - os: macos-latest
            target: x86_64-apple-darwin
            npm_pkg: darwin-x64
            bin: crewforge
          - os: macos-latest
            target: aarch64-apple-darwin
            npm_pkg: darwin-arm64
            bin: crewforge
          - os: windows-latest
            target: x86_64-pc-windows-msvc
            npm_pkg: win32-x64
            bin: crewforge.exe

    steps:
      - uses: actions/checkout@v4

      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}

      - uses: Swatinem/rust-cache@v2

      - name: Install cross (for arm64 Linux)
        if: matrix.cross
        run: cargo install cross --git https://github.com/cross-rs/cross

      - name: Build (native)
        if: '!matrix.cross'
        run: cargo build --release --target ${{ matrix.target }}

      - name: Build (cross)
        if: matrix.cross
        run: cross build --release --target ${{ matrix.target }}

      - name: Copy binary into platform package
        shell: bash
        run: |
          cp target/${{ matrix.target }}/release/${{ matrix.bin }} \
             npm/crewforge-${{ matrix.npm_pkg }}/${{ matrix.bin }}

      - name: Upload artifact
        uses: actions/upload-artifact@v4
        with:
          name: ${{ matrix.npm_pkg }}
          path: npm/crewforge-${{ matrix.npm_pkg }}/

  publish:
    name: Publish to npm
    needs: build
    runs-on: ubuntu-latest
    permissions:
      contents: write   # 创建 GitHub Release 需要
    steps:
      - uses: actions/checkout@v4

      - uses: actions/setup-node@v4
        with:
          node-version: '20'
          registry-url: 'https://registry.npmjs.org'

      - name: Extract version from tag
        run: echo "VERSION=${GITHUB_REF_NAME#v}" >> $GITHUB_ENV

      - name: Download all artifacts
        uses: actions/download-artifact@v4
        with:
          path: npm-artifacts/

      - name: Restore binaries and fix permissions
        shell: bash
        run: |
          for dir in npm-artifacts/*/; do
            pkg=$(basename "$dir")
            cp -r "$dir"* "npm/crewforge-$pkg/"
          done
          # 修复 Unix 二进制可执行位（artifact 上传/下载会丢失权限）
          chmod +x npm/crewforge-linux-x64/crewforge \
                   npm/crewforge-linux-arm64/crewforge \
                   npm/crewforge-darwin-x64/crewforge \
                   npm/crewforge-darwin-arm64/crewforge

      - name: Set version & publish platform packages
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
        run: |
          for pkg_dir in npm/crewforge-*/; do
            cd "$pkg_dir"
            npm version "$VERSION" --no-git-tag-version --allow-same-version
            npm publish --access public
            cd -
          done

      - name: Set version & publish main package
        env:
          NODE_AUTH_TOKEN: ${{ secrets.NPM_TOKEN }}
        run: |
          # 同步更新 optionalDependencies 的子包版本号，避免主包依赖旧版子包
          node -e "
            const fs = require('fs');
            const pkg = JSON.parse(fs.readFileSync('package.json', 'utf8'));
            Object.keys(pkg.optionalDependencies || {}).forEach(k => {
              pkg.optionalDependencies[k] = process.env.VERSION;
            });
            fs.writeFileSync('package.json', JSON.stringify(pkg, null, 2) + '\n');
          "
          npm version "$VERSION" --no-git-tag-version --allow-same-version
          npm publish --access public

      - name: Create GitHub Release
        env:
          GH_TOKEN: ${{ secrets.GITHUB_TOKEN }}
        run: |
          gh release create "$GITHUB_REF_NAME" \
            --title "$GITHUB_REF_NAME" \
            --generate-notes
```

## 6. 发布操作流程（人工步骤）

```bash
# 1. 确认 Cargo.toml 中 version 已更新
# 2. commit 所有改动
git commit -m "chore: bump version to 0.2.0"

# 3. 打 tag（触发 Actions 发布流）
git tag v0.2.0
git push origin main --tags
```

Actions 完成后自动发布所有 npm 包并创建 GitHub Release（release notes 由 `--generate-notes` 自动生成）。
主包和子包版本号统一从 tag 名提取，不依赖手动维护各 `package.json` 中的 `version` 字段。

## 7. 前置配置（一次性）

1. 在 npm 创建 `@crewforge` org（若尚未存在）。
2. 生成 npm Automation Token，存入 GitHub repo secrets，key 名为 `NPM_TOKEN`。
3. 首次发布若因包不存在而失败，在报错信息中确认原因后重跑即可；`--access public` 已覆盖首次发布场景，无需提前手工 publish。

## 8. 验收要点

1. `npm i -g crewforge` 在 linux-x64 / darwin-arm64 / win32-x64 上可成功安装。
2. 安装后 `crewforge --version` 输出正确版本号。
3. 不匹配平台时 JS 层打印友好错误。
4. `v*` tag 以外的 push 不触发 `release.yml`。
5. 平台子包的 `os`/`cpu` 字段正确，npm 不会在错误平台下载错误包。
