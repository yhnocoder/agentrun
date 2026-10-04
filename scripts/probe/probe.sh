#!/bin/bash
# shellcheck source-path=SCRIPTDIR

set -o pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)

PLATFORM=""
RUNTIMES=""
OUT_BASE="./probe-out"
SKIP_MODEL=0
ONLY=""

usage() {
  cat <<'EOF'
usage: probe.sh --platform macos|linux|docker|claude-cloud [--runtimes claude-code,codex,pi]
                [--out DIR] [--skip-model] [--only S2,R1,...]
EOF
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --platform) PLATFORM=$2; shift 2 ;;
    --runtimes) RUNTIMES=$2; shift 2 ;;
    --out) OUT_BASE=$2; shift 2 ;;
    --skip-model) SKIP_MODEL=1; shift ;;
    --only) ONLY=$2; shift 2 ;;
    -h|--help) usage ;;
    *) echo "unknown option: $1" >&2; usage ;;
  esac
done

case "$PLATFORM" in
  macos|linux|docker|claude-cloud) ;;
  *) echo "--platform is required: macos|linux|docker|claude-cloud" >&2; usage ;;
esac

if [ -z "$RUNTIMES" ]; then
  case "$PLATFORM" in
    claude-cloud) RUNTIMES="claude-code,pi" ;;
    *) RUNTIMES="claude-code,codex,pi" ;;
  esac
fi
RUNTIME_LIST=$(printf '%s' "$RUNTIMES" | tr ',' ' ')
for rt in $RUNTIME_LIST; do
  case "$rt" in
    claude-code|codex|pi) ;;
    *) echo "unknown runtime: $rt" >&2; exit 2 ;;
  esac
done

case "$PLATFORM" in
  macos) SANDBOX_KIND=seatbelt ;;
  *) SANDBOX_KIND=bwrap ;;
esac

. "$SCRIPT_DIR/common/util.sh"
. "$SCRIPT_DIR/common/sandbox.sh"
. "$SCRIPT_DIR/common/checks.sh"
for rt in $RUNTIME_LIST; do
  # shellcheck source=/dev/null
  . "$SCRIPT_DIR/runtimes/$rt.sh"
done

STAMP=$(date +%Y%m%d-%H%M%S)
OUT_DIR="$OUT_BASE/$PLATFORM-$STAMP"
mkdir -p "$OUT_DIR" || exit 1
OUT_DIR=$(real_dir "$OUT_DIR")
SUMMARY="$OUT_DIR/summary.md"

BASE_TMP=${TMPDIR:-/tmp}
BASE_TMP=${BASE_TMP%/}
PRIVATE_ROOT="$BASE_TMP/agentrun-probe-$(rand_hex)"
mkdir -m 0700 "$PRIVATE_ROOT" || exit 1
PRIVATE_ROOT=$(real_dir "$PRIVATE_ROOT")

[ "$PLATFORM" = docker ] && restore_docker_auth

cleanup() {
  rm -rf "$PRIVATE_ROOT"
}
trap cleanup EXIT

printf '# probe %s %s\n\n| 编号 | 运行时 | 判定 | 说明 |\n|---|---|---|---|\n' "$PLATFORM" "$STAMP" > "$SUMMARY"
write_env_txt

if [ "$PLATFORM" = macos ]; then
  selected S1 && check_seatbelt_write_s1
else
  selected S2 && check_bwrap_write_s2
  selected S3 && check_bwrap_network_s3
fi
selected S4 && check_private_tmpdir_s4
selected S5 && check_child_process_s5

if [ "$SKIP_MODEL" = 0 ]; then
  for rt in $RUNTIME_LIST; do
    if ! have "$(rt_call "$rt" bin)"; then
      for n in 1 2 3 4 5 6 7 8 9; do
        selected "R$n-$rt" && skip_check "R$n-$rt" "$rt" "$(rt_call "$rt" bin) not installed"
      done
      continue
    fi
    for entry in R1:check_basic_task_r1 R2:check_outside_write_r2 R3:check_network_r3 \
      R4:check_tmp_dirs_r4 R5:check_subagents_r5 R6:check_failure_samples_r6 \
      R7:check_sigint_r7 R8:check_no_subagents_r8 R9:check_config_isolation_r9; do
      selected "${entry%%:*}-$rt" && "${entry#*:}" "$rt"
    done
  done
  if [ "$PLATFORM" = docker ]; then
    case ",$RUNTIMES," in
      *,codex,*) if have codex; then selected D1-codex && check_codex_sandbox_in_docker_d1; else selected D1-codex && skip_check D1-codex codex "codex not installed"; fi ;;
      *) selected D1-codex && skip_check D1-codex codex "codex not in --runtimes" ;;
    esac
  fi
  if [ "$PLATFORM" = docker ] || [ "$PLATFORM" = claude-cloud ]; then
    case ",$RUNTIMES," in
      *,claude-code,*) if have claude; then selected D2-claude-code && check_claude_sandbox_in_container_d2; else selected D2-claude-code && skip_check D2-claude-code claude-code "claude not installed"; fi ;;
      *) selected D2-claude-code && skip_check D2-claude-code claude-code "claude-code not in --runtimes" ;;
    esac
  fi
  if [ "$PLATFORM" = docker ]; then
    case ",$RUNTIMES," in
      *,pi,*) if have pi; then selected D3-pi && check_bwrap_pi_in_docker_d3; else selected D3-pi && skip_check D3-pi pi "pi not installed"; fi ;;
      *) selected D3-pi && skip_check D3-pi pi "pi not in --runtimes" ;;
    esac
  fi
fi

echo
echo "output: $OUT_DIR"
