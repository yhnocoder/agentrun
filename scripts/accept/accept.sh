#!/bin/bash
# shellcheck source-path=SCRIPTDIR

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
REPO_DIR=$(cd "$SCRIPT_DIR/../.." && pwd -P)

. "$SCRIPT_DIR/lib/util.sh"
. "$SCRIPT_DIR/lib/scenarios.sh"
. "$SCRIPT_DIR/lib/concurrent.sh"

AGENTRUN=""
RUNTIMES="claude-code,pi,codex"
SANDBOX=on
LABEL=""
OUT_BASE="./accept-out"
ONLY=""
PARALLEL=3
CLAUDE_MODEL=sonnet
PI_MODEL=deepseek/deepseek-flash

usage() {
  cat <<'EOT'
usage: scripts/accept/accept.sh [--agentrun PATH] [--runtimes claude-code,pi,codex]
                                [--sandbox on|relax|off] [--label NAME] [--out DIR]
                                [--only NAME,...] [--parallel N] [--claude-model ID]
                                [--pi-model ID]
EOT
}

help() {
  usage
  cat <<'EOT'

Runs one set of scenarios through agentrun with claude-code, pi and codex, judges
each one by agentrun's exit code, its events and the files in the working
directory, and writes a summary table. Exits 1 when any scenario fails.

Options
  --agentrun PATH    agentrun under test. Default: target/debug/agentrun in this
                     repository when it is executable, else agentrun in PATH
  --runtimes LIST    comma separated runtimes to run. Default: claude-code,pi,codex
  --sandbox MODE     --sandbox value passed to every run (on, relax, off). Default: on
  --label NAME       prefix of the output directory name. Default: <macos|linux>-sandbox-<mode>
  --out DIR          parent of the output directory. Default: ./accept-out
  --only NAME,...    run only these scenarios (sandbox-check always runs, because
                     it decides which scenarios apply). Default: all except the
                     concurrent-* scenarios, which run only when listed here
  --parallel N       runs started at once by concurrent-same and concurrent-interrupt.
                     Default: 3
  --claude-model ID  --model for claude-code. Default: sonnet
  --pi-model ID      --model for pi. Default: deepseek/deepseek-flash
                     codex gets no --model and uses its own default

Scenarios
  sandbox-check dry-run usage-error relax-note basic-task outside-write session-tmp
  network-none network-custom network-full webfetch subagents no-subagents
  config-isolation fail-model fail-arg fail-credential interrupt timeout cleanup doctor
  Run only with --only, because they call the model many times at once:
  concurrent-same concurrent-same-cwd concurrent-network concurrent-interrupt concurrent-mixed
  After each of them the script checks that ~/.claude.json and the pi and codex login
  files are valid JSON, runs basic-task again with the same runtime, and checks that
  no agentrun-* directory and no runtime process is left.

Running in each environment
  macOS and Linux, in this repository:
    cargo build
    scripts/accept/accept.sh
    scripts/accept/accept.sh --sandbox off
  Cloud Managed container (no codex):
    scripts/accept/accept.sh --runtimes claude-code,pi
    scripts/accept/accept.sh --runtimes claude-code,pi --sandbox off
  docker (the image holds the three runtimes; agentrun and this directory are mounted):
    docker build -t agentrun-accept scripts/accept
    docker run --rm -v "$PWD:/src" -w /src -v agentrun-cargo:/usr/local/cargo/registry \
      rust:1-bookworm cargo build --target-dir target/linux
    docker run --rm \
      -v "$PWD/target/linux/debug/agentrun:/usr/local/bin/agentrun:ro" \
      -v "$PWD/scripts/accept:/opt/accept:ro" -v "$PWD/accept-out:/accept-out" \
      -e CLAUDE_CODE_OAUTH_TOKEN -e AGENTRUN_PI_AUTH -e AGENTRUN_CODEX_AUTH -e DEEPSEEK_API_KEY \
      agentrun-accept /opt/accept/accept.sh --agentrun /usr/local/bin/agentrun \
      --runtimes claude-code,pi --label docker-on --out /accept-out
    Run it again with --sandbox off --label docker-off and without --runtimes, so the
    three runtimes run without the sandbox. Credentials enter the container only through
    environment variables; the user's HOME is not mounted. Pass the key variable of pi's
    model service (DEEPSEEK_API_KEY above) when pi logs in with an environment variable.

Output directory: <out>/<label>-<timestamp>/
  env.txt            versions, sandbox programs and the dry-run command of each runtime
  summary.md         one table row per scenario and runtime: verdict and explanation
  <scenario>-<runtime>/
    cmd.txt          the command (shell quoted) with the prompt written as is
    stdout.jsonl     agentrun's standard output (stdout.txt for text format runs)
    stderr.txt       agentrun's standard error, including the runtime's
    exit_code, duration_ms
    raw.jsonl        the runtime's raw output (--raw)
    check.txt        verdict and explanation
Before sharing an output directory, read the stderr.txt files: the runtimes write
their own diagnostics there.
EOT
}

usage_error() {
  printf 'accept.sh: %s\n' "$1" >&2
  usage >&2
  exit 2
}

while [ $# -gt 0 ]; do
  case "$1" in
    --agentrun|--runtimes|--sandbox|--label|--out|--only|--parallel|--claude-model|--pi-model)
      [ $# -ge 2 ] || usage_error "$1 needs a value"
      ;;
  esac
  case "$1" in
    --agentrun) AGENTRUN=$2; shift 2 ;;
    --runtimes) RUNTIMES=$2; shift 2 ;;
    --sandbox) SANDBOX=$2; shift 2 ;;
    --label) LABEL=$2; shift 2 ;;
    --out) OUT_BASE=$2; shift 2 ;;
    --only) ONLY=$2; shift 2 ;;
    --parallel) PARALLEL=$2; shift 2 ;;
    --claude-model) CLAUDE_MODEL=$2; shift 2 ;;
    --pi-model) PI_MODEL=$2; shift 2 ;;
    -h|--help) help; exit 0 ;;
    *) usage_error "unknown option: $1" ;;
  esac
done

case "$SANDBOX" in
  on|relax|off) ;;
  *) usage_error "--sandbox must be on, relax or off" ;;
esac

RUNTIME_LIST=$(printf '%s' "$RUNTIMES" | tr ',' ' ')
[ -n "$RUNTIME_LIST" ] || usage_error "--runtimes is empty"
for rt in $RUNTIME_LIST; do
  case "$rt" in
    claude-code|pi|codex) ;;
    *) usage_error "unknown runtime: $rt" ;;
  esac
done

case "$PARALLEL" in
  ''|*[!0-9]*|0) usage_error "--parallel must be a positive integer" ;;
esac

ONLY_LIST=$(printf '%s' "$ONLY" | tr ',' ' ')
for name in $ONLY_LIST; do
  in_list "$name" "$ALL_SCENARIOS $CONCURRENT_SCENARIOS" || usage_error "unknown scenario: $name"
done

selected() {
  [ -z "$ONLY_LIST" ] || in_list "$1" "$ONLY_LIST"
}

if [ -z "$AGENTRUN" ]; then
  if [ -x "$REPO_DIR/target/debug/agentrun" ]; then
    AGENTRUN="$REPO_DIR/target/debug/agentrun"
  elif have agentrun; then
    AGENTRUN=$(command -v agentrun)
  else
    usage_error "agentrun not found: build it or pass --agentrun"
  fi
fi
[ -x "$AGENTRUN" ] || usage_error "not executable: $AGENTRUN"
AGENTRUN=$(absolute_path "$AGENTRUN") || usage_error "cannot resolve $AGENTRUN"

case "$(uname -s)" in
  Darwin) OS_NAME=macos; OS_SANDBOX=seatbelt ;;
  *) OS_NAME=linux; OS_SANDBOX=bubblewrap ;;
esac
[ -n "$LABEL" ] || LABEL="$OS_NAME-sandbox-$SANDBOX"

STAMP=$(date +%Y%m%d-%H%M%S)
mkdir -p "$OUT_BASE/$LABEL-$STAMP" || exit 1
OUT_DIR=$(cd "$OUT_BASE/$LABEL-$STAMP" && pwd -P)
SUMMARY="$OUT_DIR/summary.md"

BASE_TMP=${TMPDIR:-/tmp}
BASE_TMP=${BASE_TMP%/}
PRIVATE_ROOT="$BASE_TMP/agentrun-accept-$(rand_hex)"
mkdir -m 0700 "$PRIVATE_ROOT" || exit 1

cleanup() {
  rm -rf "$PRIVATE_ROOT"
}
trap cleanup EXIT

write_env_txt() {
  local rt line
  {
    printf 'uname: %s\n' "$(uname -a)"
    printf 'agentrun: %s (%s)\n' "$AGENTRUN" "$("$AGENTRUN" --version 2>&1 | head -n 1)"
    printf 'claude: %s\n' "$(version_line claude claude --version)"
    printf 'pi: %s\n' "$(version_line pi pi --version)"
    printf 'codex: %s\n' "$(version_line codex codex --version)"
    printf 'bwrap: %s\n' "$(version_line bwrap bwrap --version)"
    printf 'socat: %s\n' "$(version_line socat socat -V)"
    if [ -x /usr/bin/sandbox-exec ]; then
      printf 'sandbox-exec: /usr/bin/sandbox-exec present\n'
    else
      printf 'sandbox-exec: absent\n'
    fi
    printf 'sandbox mode: %s\n' "$SANDBOX"
    printf 'PATH: %s\n' "$PATH"
    printf 'environment variable names: %s\n' "$(env_names)"
    for rt in $RUNTIME_LIST; do
      RT=$rt
      WORK="$PRIVATE_ROOT/env-$rt"
      mkdir -p "$WORK"
      reset_wants
      dry_run_into "$PRIVATE_ROOT/env-$rt-rec"
      line=$(first_line "$LAST_OUT")
      case "$line" in
        command:*) printf '%s dry-run %s\n' "$rt" "$line" ;;
        *) printf '%s dry-run rejected (exit %s): %s\n' "$rt" "$LAST_RC" "$(end_detail)" ;;
      esac
    done
  } > "$OUT_DIR/env.txt"
}

env_names() {
  awk 'BEGIN { for (k in ENVIRON) print k }' | grep -E '^(AGENTRUN_|CLAUDE_CODE_OAUTH_TOKEN$|ANTHROPIC_|OPENAI_|CODEX_|DEEPSEEK_|PI_|HTTPS_PROXY$|https_proxy$|HTTP_PROXY$|http_proxy$|NO_PROXY$|no_proxy$|TMPDIR$)' | sort | tr '\n' ' '
}

printf '# accept %s (--sandbox %s) %s\n\n| 场景 | 运行时 | 判定 | 说明 |\n|---|---|---|---|\n' "$LABEL" "$SANDBOX" "$STAMP" > "$SUMMARY"
write_env_txt

for rt in $RUNTIME_LIST; do
  run_runtime "$rt"
done
run_concurrent_mixed

echo
echo "output: $OUT_DIR"
[ "$FAIL_COUNT" = 0 ]
