#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
hooks_dir="$repo_root/.githooks"

git -C "$repo_root" config core.hooksPath .githooks

required_hooks=(
  "commit-msg"
)

for hook in "${required_hooks[@]}"; do
  hook_path="$hooks_dir/$hook"
  if [[ ! -f "$hook_path" ]]; then
    echo "Missing hook file: $hook_path" >&2
    exit 1
  fi
  chmod +x "$hook_path"
done

echo "Installed git hooks from .githooks/"
echo "Current hooksPath: $(git -C "$repo_root" config --get core.hooksPath)"
