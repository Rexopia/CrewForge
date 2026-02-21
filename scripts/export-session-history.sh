#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  bash scripts/export-session-history.sh <session-id|session-jsonl-path> [output-md-path]

Examples:
  bash scripts/export-session-history.sh session-2026-02-21T15-07-44-700Z
  bash scripts/export-session-history.sh .room/sessions/session-2026-02-21T15-07-44-700Z.jsonl
  bash scripts/export-session-history.sh session-2026-02-21T15-07-44-700Z docs/share/david-session.md

Behavior:
  - If input is a plain session id, it resolves to .room/sessions/<session-id>.jsonl
  - If input ends with .jsonl and is not a path, it resolves to .room/sessions/<input>
  - If input is an existing file path, that file is used directly
  - Output defaults to docs/<session-id>-share.md
EOF
}

if [[ $# -lt 1 || $# -gt 2 ]]; then
  usage
  exit 1
fi

input_arg="$1"
output_arg="${2:-}"

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

if ! command -v node >/dev/null 2>&1; then
  echo "error: node is required but not found in PATH" >&2
  exit 1
fi

resolve_session_file() {
  local input="$1"

  if [[ -f "${input}" ]]; then
    echo "${input}"
    return 0
  fi

  if [[ "${input}" == */* ]]; then
    # Caller provided a path-like value but it does not exist.
    echo "${input}"
    return 0
  fi

  if [[ "${input}" == *.jsonl ]]; then
    echo ".room/sessions/${input}"
  else
    echo ".room/sessions/${input}.jsonl"
  fi
}

session_file="$(resolve_session_file "${input_arg}")"

if [[ ! -f "${session_file}" ]]; then
  echo "error: session file not found: ${session_file}" >&2
  exit 1
fi

session_basename="$(basename "${session_file}")"
session_id="${session_basename%.jsonl}"

if [[ -n "${output_arg}" ]]; then
  output_file="${output_arg}"
else
  output_file="docs/${session_id}-share.md"
fi

mkdir -p "$(dirname "${output_file}")"

node - "${session_file}" "${output_file}" "${session_id}" <<'NODE'
const fs = require("fs");

const [sessionFile, outputFile, sessionId] = process.argv.slice(2);

function chooseFence(text) {
  const candidates = ["~~~", "```", "~~~~", "````", "~~~~~", "`````"];
  for (const fence of candidates) {
    if (!text.includes(fence)) return fence;
  }
  return "~~~~~~";
}

const lines = fs
  .readFileSync(sessionFile, "utf8")
  .split(/\r?\n/)
  .filter((line) => line.trim().length > 0);

const events = lines.map((line, idx) => {
  try {
    return JSON.parse(line);
  } catch (error) {
    throw new Error(`Invalid JSON at line ${idx + 1}: ${error.message}`);
  }
});

let out = "";
out += "# CrewForge Chat History\n\n";
out += `Session ID: \`${sessionId}\`\n\n`;

for (const event of events) {
  const speaker = event.agentId
    ? `${event.speaker} (\`${event.agentId}\`)`
    : event.speaker;
  const text = typeof event.text === "string" ? event.text : String(event.text ?? "");
  const fence = chooseFence(text);

  out += `## Event ${event.eventSeq}\n`;
  out += `- Time: \`${event.ts}\`\n`;
  out += `- Role: \`${event.role}\`\n`;
  out += `- Speaker: ${speaker}\n\n`;
  out += `${fence}text\n`;
  out += text;
  if (!text.endsWith("\n")) out += "\n";
  out += `${fence}\n\n`;
}

fs.writeFileSync(outputFile, out, "utf8");
NODE

echo "Exported: ${output_file}"
