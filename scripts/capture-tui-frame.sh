#!/usr/bin/env bash
set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  bash scripts/capture-tui-frame.sh [options]

Options:
  --resume <session-id|session-jsonl-path>  Resume target (default: latest .room/sessions/session-*.jsonl)
  --demo <count>                            Run /demo <count> after startup (default: 0)
  --pageup <count>                          Send PageUp N times before capture (default: 0)
  --start-wait <seconds>                    Wait before first command (default: 2.5)
  --post-demo-wait <seconds>                Wait after /demo (default: 2.0)
  --pointsize <n>                           PNG font size (default: 16)
  --out <base-path>                         Output base path (default: /tmp/cf-tui-<timestamp>)
  --help                                    Show this help

Output:
  <base>.txt   Plain pane capture
  <base>.ansi  ANSI escape capture
  <base>.png   Rendered screenshot image
EOF
}

resume_arg=""
demo_count=0
pageup_count=0
start_wait="2.5"
post_demo_wait="2.0"
point_size=16
out_base=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --resume)
      resume_arg="${2:-}"
      shift 2
      ;;
    --demo)
      demo_count="${2:-0}"
      shift 2
      ;;
    --pageup)
      pageup_count="${2:-0}"
      shift 2
      ;;
    --start-wait)
      start_wait="${2:-2.5}"
      shift 2
      ;;
    --post-demo-wait)
      post_demo_wait="${2:-2.0}"
      shift 2
      ;;
    --pointsize)
      point_size="${2:-16}"
      shift 2
      ;;
    --out)
      out_base="${2:-}"
      shift 2
      ;;
    --help)
      usage
      exit 0
      ;;
    *)
      echo "error: unknown option: $1" >&2
      usage
      exit 1
      ;;
  esac
done

if ! command -v tmux >/dev/null 2>&1; then
  echo "error: tmux not found in PATH" >&2
  exit 1
fi
if ! command -v convert >/dev/null 2>&1; then
  echo "error: ImageMagick convert not found in PATH" >&2
  exit 1
fi

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
repo_root="$(cd "${script_dir}/.." && pwd)"
cd "${repo_root}"

resolve_resume_arg() {
  local input="$1"
  if [[ -n "${input}" ]]; then
    if [[ -f "${input}" ]]; then
      echo "${input}"
      return 0
    fi
    if [[ "${input}" == *.jsonl ]]; then
      echo ".room/sessions/${input}"
      return 0
    fi
    echo "${input}"
    return 0
  fi

  local latest_jsonl
  latest_jsonl="$(find .room/sessions -maxdepth 1 -type f -name 'session-*.jsonl' | sort | tail -n 1)"
  if [[ -z "${latest_jsonl}" ]]; then
    echo "error: no session jsonl found under .room/sessions; pass --resume explicitly" >&2
    exit 1
  fi
  echo "${latest_jsonl}"
}

resume_value="$(resolve_resume_arg "${resume_arg}")"
if [[ "${resume_value}" == */* ]]; then
  if [[ ! -f "${resume_value}" ]]; then
    echo "error: resume target file not found: ${resume_value}" >&2
    exit 1
  fi
fi

if [[ -z "${out_base}" ]]; then
  ts="$(date -u +%Y%m%dT%H%M%SZ)"
  out_base="/tmp/cf-tui-${ts}"
fi

mkdir -p "$(dirname "${out_base}")"

session_name="cf_tui_capture_$$"
cleanup() {
  if tmux has-session -t "${session_name}" 2>/dev/null; then
    tmux kill-session -t "${session_name}" >/dev/null 2>&1 || true
  fi
}
trap cleanup EXIT

tmux new-session -d -s "${session_name}" \
  "cd '${repo_root}' && cargo run --manifest-path crewforge-rs/Cargo.toml -- chat --dry-run --resume '${resume_value}'"

sleep "${start_wait}"

submit_tui_command() {
  local cmd="$1"
  tmux send-keys -t "${session_name}" "${cmd}"
  # Avoid the paste-burst Enter suppression window in TUI.
  sleep 0.5
  tmux send-keys -t "${session_name}" Enter
}

if [[ "${demo_count}" -gt 0 ]]; then
  submit_tui_command "/demo ${demo_count}"
  sleep "${post_demo_wait}"
fi

if [[ "${pageup_count}" -gt 0 ]]; then
  for ((i = 0; i < pageup_count; i++)); do
    tmux send-keys -t "${session_name}" PageUp
  done
  sleep 0.4
fi

tmux capture-pane -pt "${session_name}:0.0" -S -220 > "${out_base}.txt"
tmux capture-pane -pt "${session_name}:0.0" -e -S -220 > "${out_base}.ansi"

convert \
  -background '#0b1020' \
  -fill '#dbe7ff' \
  -font 'DejaVu-Sans-Mono' \
  -pointsize "${point_size}" \
  "text:${out_base}.txt" \
  "${out_base}.png"

submit_tui_command "/exit"
sleep 0.4

echo "Captured:"
echo "  ${out_base}.txt"
echo "  ${out_base}.ansi"
echo "  ${out_base}.png"

