#!/bin/bash
# shellcheck source-path=SCRIPTDIR

set -o pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)

SANDBOX=on
LABEL=""
RUNTIMES="claude-code,codex,pi"
OUT_BASE="./probe-out"
SKIP_MODEL=0
ONLY=""

usage() {
  cat <<'EOF'
usage: probe.sh [--sandbox on|off] [--label NAME] [--runtimes claude-code,codex,pi]
                [--out DIR] [--skip-model] [--only NAME,...]
EOF
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --sandbox) SANDBOX=$2; shift 2 ;;
    --label) LABEL=$2; shift 2 ;;
    --runtimes) RUNTIMES=$2; shift 2 ;;
    --out) OUT_BASE=$2; shift 2 ;;
    --skip-model) SKIP_MODEL=1; shift ;;
    --only) ONLY=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown option: $1" >&2; usage ;;
  esac
done

case "$SANDBOX" in
  on|off) ;;
  *) echo "--sandbox must be on or off" >&2; usage ;;
esac

RUNTIME_LIST=$(printf '%s' "$RUNTIMES" | tr ',' ' ')
for rt in $RUNTIME_LIST; do
  case "$rt" in
    claude-code|codex|pi) ;;
    *) echo "unknown runtime: $rt" >&2; exit 2 ;;
  esac
done

case "$(uname -s)" in
  Darwin) OS_NAME=macos; SANDBOX_KIND=seatbelt ;;
  *) OS_NAME=linux; SANDBOX_KIND=bwrap ;;
esac
[ -z "$LABEL" ] && LABEL="$OS_NAME-sandbox-$SANDBOX"

. "$SCRIPT_DIR/common/util.sh"
. "$SCRIPT_DIR/common/sandbox.sh"
. "$SCRIPT_DIR/common/checks.sh"
for rt in claude-code codex pi; do
  # shellcheck source=/dev/null
  . "$SCRIPT_DIR/runtimes/$rt.sh"
done

PI_STATE_DIR=${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}
PI_PROVIDER=""
PI_MODEL_HOST=""
if [ -f "$PI_STATE_DIR/settings.json" ]; then
  PI_PROVIDER=$(node -e 'try { process.stdout.write(String(JSON.parse(require("fs").readFileSync(process.argv[1], "utf8")).defaultProvider || "")); } catch (e) {}' "$PI_STATE_DIR/settings.json")
fi
case "$PI_PROVIDER" in
  deepseek) PI_MODEL_HOST=api.deepseek.com ;;
  anthropic) PI_MODEL_HOST=api.anthropic.com ;;
  openai) PI_MODEL_HOST=api.openai.com ;;
  google) PI_MODEL_HOST=generativelanguage.googleapis.com ;;
  openrouter) PI_MODEL_HOST=openrouter.ai ;;
esac

STAMP=$(date +%Y%m%d-%H%M%S)
OUT_DIR="$OUT_BASE/$LABEL-$STAMP"
mkdir -p "$OUT_DIR" || exit 1
OUT_DIR=$(real_dir "$OUT_DIR")
SUMMARY="$OUT_DIR/summary.md"

BASE_TMP=${TMPDIR:-/tmp}
BASE_TMP=${BASE_TMP%/}
PRIVATE_ROOT="$BASE_TMP/agentrun-probe-$(rand_hex)"
mkdir -m 0700 "$PRIVATE_ROOT" || exit 1

write_credentials
unset AGENTRUN_CODEX_AUTH AGENTRUN_PI_AUTH

PI_AUTH_TARGET=""
[ -f "$PI_STATE_DIR/auth.json" ] && PI_AUTH_TARGET=$(real_file "$PI_STATE_DIR/auth.json")
CODEX_AUTH_TARGET=""
[ -f "${CODEX_HOME:-$HOME/.codex}/auth.json" ] && CODEX_AUTH_TARGET=$(real_file "${CODEX_HOME:-$HOME/.codex}/auth.json")

cleanup() {
  stop_proxy
  [ -n "$SESSION_TMP" ] && rm -rf "$SESSION_TMP"
  rm -rf "$PRIVATE_ROOT"
}
trap cleanup EXIT

printf '# probe %s (--sandbox %s, %s) %s\n\n| 检查 | 运行时 | 判定 | 说明 |\n|---|---|---|---|\n' "$LABEL" "$SANDBOX" "$SANDBOX_KIND" "$STAMP" > "$SUMMARY"
write_env_txt

if [ "$SANDBOX" = on ]; then
  check_sandbox_preflight
  run_sandbox_checks
fi

if [ "$SKIP_MODEL" = 0 ]; then
  for rt in $RUNTIME_LIST; do
    run_model_checks_for_runtime "$rt"
  done
fi

echo
echo "output: $OUT_DIR"
