#!/usr/bin/env bash
set -euo pipefail

subject="${1:-}"

if [[ -z "$subject" ]]; then
  echo "commit subject is empty" >&2
  exit 1
fi

# Conventional Commits with optional scope and optional breaking marker.
# Examples:
#   feat: add history pager
#   fix(tui): keep cursor visible
#   refactor(chat)!: simplify startup banner flow
conventional_regex='^(feat|fix|docs|chore|refactor|perf|test|build|ci|style|revert)(\([[:alnum:]./_-]+\))?!?: .+'

# Allow git-generated merge commit titles and revert commit titles.
if [[ "$subject" =~ $conventional_regex ]] || [[ "$subject" == Merge\ * ]] || [[ "$subject" == Revert\ \"*\" ]]; then
  exit 0
fi

cat >&2 <<'EOF'
Invalid commit subject.
Expected Conventional Commits format:
  <type>(optional-scope): <description>

Examples:
  feat: add session history pager
  fix(tui): keep input cursor visible
  docs: update release notes
EOF

echo "Actual: $subject" >&2
exit 1
