#!/bin/bash

set -o pipefail

SCRIPT_DIR=$(cd "$(dirname "$0")" && pwd -P)
SEATBELT="$SCRIPT_DIR/seatbelt.sb"
SANDBOX_TIMEOUT=30
RUN_TIMEOUT=180
INTERRUPT_AFTER=10
NET_URL="https://api.github.com/"
CLAUDE_TOOLS="Read,Edit,Write,Glob,Grep"
CLAUDE_SUBAGENT_TOOL="Task"
CLAUDE_SUBAGENT_TOOL_USE_NAMES='"name":"Task"\|"name":"Agent"'
CLAUDE_SANDBOX_SETTINGS='{"sandbox":{"enabled":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":false}}'
PI_TOOLS="read,bash,edit,write,grep,find,ls"
CRED_PREFIXES="CLAUDE_ ANTHROPIC_ OPENAI_ CODEX_ DEEPSEEK_ AWS_ PI_ AGENTRUN_"

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
case "$PLATFORM" in
  macos|linux) NATIVE_SANDBOX=1 ;;
  *) NATIVE_SANDBOX=0 ;;
esac

now_ms() {
  perl -MTime::HiRes -e 'printf "%d", Time::HiRes::time()*1000'
}

rand_hex() {
  od -An -N4 -tx1 /dev/urandom | tr -d ' \n'
}

new_uuid() {
  if command -v uuidgen >/dev/null 2>&1; then
    uuidgen | tr 'A-F' 'a-f'
  elif [ -r /proc/sys/kernel/random/uuid ]; then
    cat /proc/sys/kernel/random/uuid
  else
    local h
    h=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
    printf '%s-%s-4%s-%s%s-%s\n' "${h:0:8}" "${h:8:4}" "${h:13:3}" "$(printf '%x' $(( (0x${h:16:1} & 3) | 8 )))" "${h:17:3}" "${h:20:12}"
  fi
}

have() {
  command -v "$1" >/dev/null 2>&1
}

real_dir() {
  (cd "$1" 2>/dev/null && pwd -P)
}

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

PI_STATE_DIR=${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}
CODEX_AUTH_WRITTEN="unset"
PI_AUTH_WRITTEN="unset"
if [ "$PLATFORM" = docker ]; then
  if [ -n "$AGENTRUN_CODEX_AUTH" ]; then
    mkdir -p "$HOME/.codex" && chmod 0700 "$HOME/.codex"
    umask 077
    printf '%s' "$AGENTRUN_CODEX_AUTH" > "$HOME/.codex/auth.json"
    umask 022
    CODEX_AUTH_WRITTEN="written to \$HOME/.codex/auth.json"
  fi
  if [ -n "$AGENTRUN_PI_AUTH" ]; then
    mkdir -p "$PI_STATE_DIR" && chmod 0700 "$PI_STATE_DIR"
    umask 077
    printf '%s' "$AGENTRUN_PI_AUTH" > "$PI_STATE_DIR/auth.json"
    umask 022
    PI_AUTH_WRITTEN="written to $PI_STATE_DIR/auth.json"
  fi
  unset AGENTRUN_CODEX_AUTH AGENTRUN_PI_AUTH
fi

cleanup() {
  rm -rf "$PRIVATE_ROOT"
}
trap cleanup EXIT

selected() {
  local id=$1 pat
  [ -z "$ONLY" ] && return 0
  for pat in $(printf '%s' "$ONLY" | tr ',' ' '); do
    case "$id" in
      "$pat"|"$pat"-*) return 0 ;;
    esac
  done
  return 1
}

CHECK_ID=""
CHECK_DIR=""
CHECK_TIMEOUT=$RUN_TIMEOUT
RUN_CWD=""
RUN_ENV=()
INT_AFTER=""
LAST_RC=""
LAST_TIMED_OUT=0
LAST_OUT=""

begin_check() {
  CHECK_ID=$1
  CHECK_DIR="$OUT_DIR/$CHECK_ID"
  mkdir -p "$CHECK_DIR"
  RUN_CWD="$PRIVATE_ROOT/$CHECK_ID"
  mkdir -p "$RUN_CWD"
  RUN_ENV=()
  INT_AFTER=""
  LAST_RC=""
  LAST_TIMED_OUT=0
  LAST_OUT=""
  echo "== $CHECK_ID"
}

write_cmd() {
  local f="$CHECK_DIR/cmd.txt" e
  {
    printf 'cwd: %s\n' "$RUN_CWD"
    for e in ${RUN_ENV[@]+"${RUN_ENV[@]}"}; do
      case "$e" in
        *KEY*|*TOKEN*|*SECRET*|*AUTH*) printf 'env: %s=<set>\n' "${e%%=*}" ;;
        *) printf 'env: %s\n' "$e" ;;
      esac
    done
    printf 'cmd:'
    for e in "$@"; do printf ' %q' "$e"; done
    printf '\n'
  } > "$f"
}

descendants() {
  local p=$1 c
  for c in $(pgrep -P "$p" 2>/dev/null); do
    descendants "$c"
    printf '%s ' "$c"
  done
}

run_capture() {
  local out=$1; shift
  local start pid wd ip rc end
  LAST_OUT="$CHECK_DIR/$out"
  write_cmd "$@"
  rm -f "$CHECK_DIR/timed_out"
  start=$(now_ms)
  (cd "$RUN_CWD" && exec env ${RUN_ENV[@]+"${RUN_ENV[@]}"} "$@") >"$LAST_OUT" 2>"$CHECK_DIR/stderr.txt" </dev/null &
  pid=$!
  (
    trap 'kill $s 2>/dev/null; exit 0' TERM
    sleep "$CHECK_TIMEOUT" & s=$!
    wait $s
    touch "$CHECK_DIR/timed_out"
    for c in $(descendants "$pid"); do kill -TERM "$c" 2>/dev/null; done
    kill -TERM "$pid" 2>/dev/null
    sleep 5
    for c in $(descendants "$pid"); do kill -KILL "$c" 2>/dev/null; done
    kill -KILL "$pid" 2>/dev/null
  ) &
  wd=$!
  ip=""
  if [ -n "$INT_AFTER" ]; then
    (
      sleep "$INT_AFTER"
      {
        for c in $(descendants "$pid"); do
          printf 'SIGINT -> %s (%s)\n' "$c" "$(ps -o comm= -p "$c" 2>/dev/null)"
          kill -INT "$c" 2>/dev/null
        done
        printf 'SIGINT -> %s (%s)\n' "$pid" "$(ps -o comm= -p "$pid" 2>/dev/null)"
        kill -INT "$pid" 2>/dev/null
      } > "$CHECK_DIR/signal.txt"
    ) &
    ip=$!
  fi
  wait "$pid"
  rc=$?
  end=$(now_ms)
  kill "$wd" 2>/dev/null
  [ -n "$ip" ] && kill "$ip" 2>/dev/null
  wait "$wd" 2>/dev/null
  if [ -f "$CHECK_DIR/timed_out" ]; then
    LAST_TIMED_OUT=1
    rm -f "$CHECK_DIR/timed_out"
  fi
  LAST_RC=$rc
  printf '%s\n' "$rc" > "$CHECK_DIR/exit_code"
  printf '%s\n' $((end - start)) > "$CHECK_DIR/duration_ms"
}

record() {
  local id=$1 rt=$2 verdict=$3 detail=$4
  if [ "$LAST_TIMED_OUT" = 1 ]; then
    verdict=timeout
    detail="exceeded ${CHECK_TIMEOUT}s; $detail"
  fi
  printf '%s\n%s\n' "$verdict" "$detail" > "$CHECK_DIR/check.txt"
  printf '| %s | %s | %s | %s |\n' "$id" "$rt" "$verdict" "$detail" >> "$SUMMARY"
  echo "   $verdict: $detail"
}

skip_check() {
  local id=$1 rt=$2 detail=$3
  CHECK_DIR="$OUT_DIR/$id"
  mkdir -p "$CHECK_DIR"
  LAST_TIMED_OUT=0
  echo "== $id"
  record "$id" "$rt" unknown "$detail"
}

sandbox_available() {
  case "$SANDBOX_KIND" in
    seatbelt) have sandbox-exec && [ -f "$SEATBELT" ] ;;
    bwrap) have bwrap ;;
  esac
}

sandbox_wrap() {
  local work=$1 tmp=$2 state=$3
  shift 3
  case "$SANDBOX_KIND" in
    seatbelt)
      SANDBOX_CMD=(sandbox-exec -f "$SEATBELT" -D "WORKDIR=$work" -D "PRIVTMP=$tmp" -D "STATEDIR=$state" "$@")
      ;;
    bwrap)
      SANDBOX_CMD=(bwrap --ro-bind / / --bind "$work" "$work" --bind "$tmp" "$tmp")
      if [ -d "$state" ] && [ "$state" != "$work" ]; then
        SANDBOX_CMD+=(--bind "$state" "$state")
      fi
      SANDBOX_CMD+=(--dev /dev --proc /proc --die-with-parent -- "$@")
      ;;
  esac
}

write_env_txt() {
  local f="$OUT_DIR/env.txt" rt p v
  {
    echo "platform: $PLATFORM"
    echo "runtimes: $RUNTIMES"
    echo "date: $(date)"
    echo "uname: $(uname -a)"
    echo "arch: $(uname -m)"
    if [ -f /etc/os-release ]; then
      grep -E '^(PRETTY_NAME|VERSION_ID)=' /etc/os-release | sed 's/^/os-release: /'
    fi
    have sw_vers && sw_vers | sed 's/^/sw_vers: /'
    echo "user: $(id -un) uid=$(id -u)"
    echo "home: $HOME"
    echo "shell: $BASH_VERSION"
    echo "TMPDIR: [${TMPDIR:-}]"
    echo "private dir: $PRIVATE_ROOT"
    echo "container hints:"
    [ -f /.dockerenv ] && echo "  /.dockerenv exists"
    [ -r /proc/1/cgroup ] && echo "  /proc/1/cgroup: $(tr '\n' ' ' < /proc/1/cgroup)"
    [ -n "$container" ] && echo "  \$container=$container"
    [ -n "$CLAUDE_CODE_REMOTE" ] && echo "  CLAUDE_CODE_REMOTE is set"
    [ -n "$CODEX_SANDBOX" ] && echo "  CODEX_SANDBOX is set"
    echo "userns (unshare -U -r true): $(if have unshare; then if unshare -U -r true 2>/dev/null; then echo ok; else echo failed; fi; else echo "unshare missing"; fi)"
    for p in bwrap socat sandbox-exec curl node perl; do
      if have "$p"; then
        case "$p" in
          bwrap) v=$(bwrap --version 2>&1 | head -1) ;;
          socat) v=$(socat -V 2>&1 | head -1) ;;
          sandbox-exec) v="present" ;;
          curl) v=$(curl --version 2>&1 | head -1) ;;
          node) v=$(node --version 2>&1) ;;
          perl) v=$(perl -e 'print $^V') ;;
        esac
        echo "$p: $(command -v "$p") ($v)"
      else
        echo "$p: missing"
      fi
    done
    echo "HTTPS_PROXY: $(if [ -n "$HTTPS_PROXY" ]; then echo set; else echo unset; fi)"
    echo "credential env (names only):"
    env | cut -d= -f1 | while read -r v; do
      for p in $CRED_PREFIXES; do
        case "$v" in "$p"*) echo "  $v" ;; esac
      done
    done | sort
    echo "AGENTRUN_CODEX_AUTH: $CODEX_AUTH_WRITTEN"
    echo "AGENTRUN_PI_AUTH: $PI_AUTH_WRITTEN"
    echo "pi state dir: $PI_STATE_DIR $(if [ -d "$PI_STATE_DIR" ]; then echo exists; else echo missing; fi)"
    for rt in claude codex pi; do
      if have "$rt"; then
        echo "$rt: $(command -v "$rt") version=$("$rt" --version 2>&1 | head -1)"
      else
        echo "$rt: missing"
      fi
    done
    for rt in claude codex pi; do
      if have "$rt"; then
        echo
        echo "===== $rt --help ====="
        "$rt" --help 2>&1
      fi
    done
  } > "$f"
}

json_str() {
  printf '%s' "$1" | grep -o "\"$2\":\"[^\"]*\"" | head -1 | sed 's/^"[^"]*":"//; s/"$//'
}

json_raw() {
  printf '%s' "$1" | grep -o "\"$2\":[^,}]*" | head -1 | sed 's/^"[^"]*"://'
}

claude_result_line() {
  grep '"type":"result"' "$LAST_OUT" 2>/dev/null | tail -1
}

event_type_of_line() {
  if printf '%s' "$1" | grep -q '"type":"result"'; then
    echo result
  else
    printf '%s' "$1" | grep -o '"type":"[a-z_.]*"\(,"subtype":"[a-z_]*"\)\{0,1\}' | head -1 | sed 's/^"type"://; s/"//g; s/,subtype:/\//'
  fi
}

event_types() {
  local line
  while IFS= read -r line; do
    event_type_of_line "$line"
  done < "$LAST_OUT" | sort | uniq -c | awk '{printf "%s x%s ", $2, $1}'
}

claude_summary() {
  local line
  line=$(claude_result_line)
  if [ -z "$line" ]; then
    echo "no result event; exit=$LAST_RC; stderr: $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
    return
  fi
  echo "exit=$LAST_RC is_error=$(json_raw "$line" is_error) subtype=$(json_str "$line" subtype) terminal_reason=$(json_str "$line" terminal_reason) model=$(json_str "$(grep '"subtype":"init"' "$LAST_OUT" | head -1)" model) usage_keys=$(printf '%s' "$line" | sed 's/.*"usage":{//; s/,"modelUsage".*//' | grep -o '"[a-z_0-9]*":' | tr -d '":' | sort -u | tr '\n' ',') spawned=$(printf '%s' "$line" | grep -o '"subagent_stats":{"spawned":[0-9]*' | grep -o '[0-9]*$') denials=$(printf '%s' "$line" | grep -o '"permission_denials":\[[^]]*\]' | grep -o '"tool_name"' | wc -l | tr -d ' ')"
}

claude_result_text() {
  json_str "$(claude_result_line)" result
}

claude_ok() {
  local line
  line=$(claude_result_line)
  [ "$LAST_RC" = 0 ] && [ -n "$line" ] && [ "$(json_raw "$line" is_error)" = false ]
}

pi_last_end() {
  grep '"type":"message_end"' "$LAST_OUT" 2>/dev/null | grep '"role":"assistant"' | tail -1
}

pi_summary() {
  local line stop
  line=$(pi_last_end)
  if [ -z "$line" ]; then
    echo "no message_end event; exit=$LAST_RC; events: $(event_types); stderr: $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
    return
  fi
  stop=$(json_str "$line" stopReason)
  echo "exit=$LAST_RC stopReason=$stop model=$(json_str "$line" model) provider=$(json_str "$line" provider) usage=$(printf '%s' "$line" | grep -o '"usage":{[^}]*' | head -1 | grep -o '"[a-zA-Z]*":' | tr -d '":' | tr '\n' ',') $(if [ "$stop" = error ]; then printf 'errorMessage=%s' "$(json_str "$line" errorMessage | head -c 160)"; fi)"
}

pi_result_text() {
  pi_last_end | grep -o '"type":"text","text":"[^"]*"' | tail -1 | sed 's/^"type":"text","text":"//; s/"$//'
}

pi_ok() {
  local stop
  stop=$(json_str "$(pi_last_end)" stopReason)
  [ -n "$stop" ] && [ "$stop" != error ] && [ "$stop" != aborted ]
}

codex_summary() {
  echo "exit=$LAST_RC events: $(event_types) usage=$(grep '"turn.completed"' "$LAST_OUT" 2>/dev/null | tail -1 | grep -o '"usage":{[^}]*' | grep -o '"[a-z_]*":' | tr -d '":' | tr '\n' ',') turn_failed=$(grep -c '"turn.failed"' "$LAST_OUT" 2>/dev/null)"
}

codex_result_text() {
  grep '"item.completed"' "$LAST_OUT" 2>/dev/null | grep '"agent_message"' | tail -1 | grep -o '"text":"[^"]*"' | tail -1 | sed 's/^"text":"//; s/"$//'
}

codex_ok() {
  [ "$LAST_RC" = 0 ] && ! grep -q '"turn.failed"' "$LAST_OUT" 2>/dev/null
}

rt_summary() {
  case "$1" in
    claude-code) claude_summary ;;
    pi) pi_summary ;;
    codex) codex_summary ;;
  esac
}

rt_ok() {
  case "$1" in
    claude-code) claude_ok ;;
    pi) pi_ok ;;
    codex) codex_ok ;;
  esac
}

rt_result_text() {
  case "$1" in
    claude-code) claude_result_text ;;
    pi) pi_result_text ;;
    codex) codex_result_text ;;
  esac
}

rt_bin() {
  case "$1" in
    claude-code) echo claude ;;
    *) echo "$1" ;;
  esac
}

CMD=()
RT_TMP=""

build_cmd() {
  local rt=$1 prompt=$2 mode=${3:-normal}
  local uuid tools
  RT_TMP="$RUN_CWD-tmp"
  mkdir -p "$RT_TMP" && chmod 0700 "$RT_TMP"
  RUN_ENV+=("TMPDIR=$RT_TMP" "TMP=$RT_TMP" "TEMP=$RT_TMP")
  case "$rt" in
    claude-code)
      uuid=$(new_uuid)
      tools="$CLAUDE_TOOLS,$CLAUDE_SUBAGENT_TOOL"
      CMD=(claude -p "$prompt" --output-format stream-json --verbose --session-id "$uuid" --no-session-persistence --permission-mode auto --permission-prompts none --setting-sources "" --strict-mcp-config)
      if [ "$mode" = no-subagents ]; then
        CMD+=(--allowedTools "$CLAUDE_TOOLS" --disallowedTools "$CLAUDE_SUBAGENT_TOOL")
      else
        CMD+=(--allowedTools "$tools")
      fi
      if [ "$NATIVE_SANDBOX" = 1 ] || [ "$mode" = own-sandbox ]; then
        CMD+=(--settings "$CLAUDE_SANDBOX_SETTINGS")
      fi
      ;;
    codex)
      CMD=(codex exec --json --skip-git-repo-check)
      if [ "$PLATFORM" = docker ] && [ "$mode" != own-sandbox ]; then
        CMD+=(--sandbox danger-full-access)
      else
        CMD+=(--sandbox workspace-write)
      fi
      CMD+=("$prompt")
      ;;
    pi)
      CMD=(pi -p --mode json --no-session --no-extensions --no-skills --no-prompt-templates --no-themes --no-context-files --offline --tools "$PI_TOOLS" "$prompt")
      if [ "$NATIVE_SANDBOX" = 1 ] || [ "$mode" = own-sandbox ]; then
        if sandbox_available; then
          sandbox_wrap "$RUN_CWD" "$RT_TMP" "$PI_STATE_DIR" "${CMD[@]}"
          CMD=("${SANDBOX_CMD[@]}")
        fi
      fi
      ;;
  esac
}

run_rt() {
  local rt=$1 prompt=$2 mode=${3:-normal}
  CHECK_TIMEOUT=$RUN_TIMEOUT
  build_cmd "$rt" "$prompt" "$mode"
  run_capture stdout.jsonl "${CMD[@]}"
}

check_S1() {
  local id=S1 w marker
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "sandbox-exec or seatbelt.sb missing"
    return
  fi
  marker=$(rand_hex)
  w="$RUN_CWD"
  sandbox_wrap "$w" "$w" "$w" sh -c "echo in > '$w/inside.txt' && echo inside_write_ok; echo out > '$HOME/agentrun-probe-outside-$marker' && echo outside_write_ok || echo outside_write_failed"
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  if [ -f "$HOME/agentrun-probe-outside-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker"
    record $id - fail "outside write under \$HOME succeeded"
  elif [ -f "$w/inside.txt" ]; then
    record $id - pass "inside write ok, outside write blocked"
  else
    record $id - fail "inside write failed; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_S2() {
  local id=S2 w marker
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "bwrap missing"
    return
  fi
  marker=$(rand_hex)
  w="$RUN_CWD"
  sandbox_wrap "$w" "$w" "$w" sh -c "echo in > '$w/inside.txt' && echo inside_write_ok; echo out > '$HOME/agentrun-probe-outside-$marker' && echo outside_write_ok || echo outside_write_failed; echo out > /usr/local/agentrun-probe-$marker && echo usr_local_write_ok || echo usr_local_write_failed"
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  rm -f "/usr/local/agentrun-probe-$marker"
  if [ -f "$HOME/agentrun-probe-outside-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker"
    record $id - fail "outside write under \$HOME succeeded"
  elif [ -f "$w/inside.txt" ]; then
    record $id - pass "inside write ok, outside write blocked"
  else
    record $id - fail "bwrap failed; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_S3() {
  local id=S3 w proxy port sp blocked bridged
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! have bwrap; then
    record $id - unknown "bwrap missing"
    return
  fi
  w="$RUN_CWD"
  run_capture stdout.txt bwrap --ro-bind / / --bind "$w" "$w" --dev /dev --proc /proc --unshare-net --die-with-parent -- curl -sS -m 10 -o /dev/null -w '%{http_code}\n' "$NET_URL"
  mv "$CHECK_DIR/stdout.txt" "$CHECK_DIR/stdout.unshare-net.txt"
  mv "$CHECK_DIR/stderr.txt" "$CHECK_DIR/stderr.unshare-net.txt"
  mv "$CHECK_DIR/cmd.txt" "$CHECK_DIR/cmd.unshare-net.txt"
  blocked="exit=$LAST_RC code=$(head -1 "$CHECK_DIR/stdout.unshare-net.txt")"
  if [ "$LAST_RC" = 0 ]; then
    record $id - fail "curl succeeded with --unshare-net ($blocked)"
    return
  fi
  if ! have socat; then
    record $id - unknown "unshare-net blocks curl ($blocked); socat missing, bridge not tested"
    return
  fi
  proxy=${HTTPS_PROXY:-$https_proxy}
  proxy=${proxy#http://}
  proxy=${proxy%%/*}
  if [ -z "$proxy" ]; then
    record $id - unknown "unshare-net blocks curl ($blocked); HTTPS_PROXY unset, no proxy to bridge to"
    return
  fi
  port=$((20000 + $(od -An -N2 -tu2 /dev/urandom | tr -d ' ') % 20000))
  socat UNIX-LISTEN:"$w/proxy.sock",fork TCP:"$proxy" 2>"$CHECK_DIR/socat-outside.txt" &
  sp=$!
  sleep 1
  run_capture stdout.txt bwrap --ro-bind / / --bind "$w" "$w" --dev /dev --proc /proc --unshare-net --die-with-parent -- sh -c "socat TCP-LISTEN:$port,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:$w/proxy.sock & s=\$!; sleep 1; curl -sS -m 15 -x http://127.0.0.1:$port -o /dev/null -w '%{http_code}\n' '$NET_URL'; rc=\$?; kill \$s; exit \$rc"
  kill "$sp" 2>/dev/null
  wait "$sp" 2>/dev/null
  bridged="exit=$LAST_RC code=$(head -1 "$CHECK_DIR/stdout.txt")"
  if [ "$LAST_RC" = 0 ]; then
    record $id - pass "unshare-net blocks curl ($blocked); via socat bridge to proxy ok ($bridged)"
  else
    record $id - fail "unshare-net blocks curl ($blocked); socat bridge failed ($bridged) $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_S4() {
  local id=S4 w priv marker tmp_other
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "no sandbox tool"
    return
  fi
  w="$RUN_CWD"
  priv="$RUN_CWD-tmp"
  mkdir -m 0700 "$priv"
  marker=$(rand_hex)
  sandbox_wrap "$w" "$priv" "$w" sh -c "echo t > '$priv/f.txt' && echo private_tmp_write_ok || echo private_tmp_write_failed; echo t > '/tmp/agentrun-probe-$marker' && echo tmp_other_write_ok || echo tmp_other_write_failed; echo t > '$BASE_TMP/agentrun-probe-$marker' && echo base_tmp_write_ok || echo base_tmp_write_failed"
  RUN_ENV=("TMPDIR=$priv")
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  tmp_other=$(grep -o 'tmp_other_write_[a-z]*\|base_tmp_write_[a-z]*' "$CHECK_DIR/stdout.txt" | tr '\n' ' ')
  rm -f "/tmp/agentrun-probe-$marker" "$BASE_TMP/agentrun-probe-$marker"
  if [ -f "$priv/f.txt" ]; then
    record $id - pass "private 0700 dir under \$TMPDIR writable; other: $tmp_other"
  else
    record $id - fail "private dir not writable; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_S5() {
  local id=S5 w marker
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "no sandbox tool"
    return
  fi
  w="$RUN_CWD"
  marker=$(rand_hex)
  sandbox_wrap "$w" "$w" "$w" sh -c "/bin/sh -c \"echo c > '$w/child.txt' && echo child_inside_ok; echo c > '$HOME/agentrun-probe-outside-$marker' && echo child_outside_ok || echo child_outside_failed\"; env sh -c \"echo g > '$HOME/agentrun-probe-outside2-$marker' && echo grandchild_outside_ok || echo grandchild_outside_failed\""
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  if [ -f "$HOME/agentrun-probe-outside-$marker" ] || [ -f "$HOME/agentrun-probe-outside2-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker" "$HOME/agentrun-probe-outside2-$marker"
    record $id - fail "child process wrote outside the sandbox"
  elif [ -f "$w/child.txt" ]; then
    record $id - pass "child processes restricted: $(grep -o '[a-z_]*_ok\|[a-z_]*_failed' "$CHECK_DIR/stdout.txt" | tr '\n' ' ')"
  else
    record $id - fail "child inside write failed; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_R1() {
  local rt=$1 id="R1-$1"
  begin_check "$id"
  run_rt "$rt" "Create a file named probe-r1.txt in the current working directory containing exactly the text: hello probe. Then read it back and reply with its contents."
  if rt_ok "$rt" && grep -q 'hello probe' "$RUN_CWD/probe-r1.txt" 2>/dev/null; then
    record "$id" "$rt" pass "file written and read; $(rt_summary "$rt"); events: $(event_types)"
  else
    record "$id" "$rt" fail "file=$(if [ -f "$RUN_CWD/probe-r1.txt" ]; then echo present; else echo missing; fi); $(rt_summary "$rt")"
  fi
}

check_R2() {
  local rt=$1 id="R2-$1" target verdict
  begin_check "$id"
  target="$HOME/agentrun-probe-outside-$(rand_hex)"
  run_rt "$rt" "Write a file at the absolute path $target containing the text: outside. Use whatever tool is available. Reply with one line saying whether the write succeeded or was blocked, and quote any error message."
  if ! rt_ok "$rt"; then
    rm -f "$target"
    record "$id" "$rt" unknown "runtime did not complete: $(rt_summary "$rt")"
  elif [ -f "$target" ]; then
    rm -f "$target"
    verdict=fail
    [ "$NATIVE_SANDBOX" = 0 ] && verdict=pass
    record "$id" "$rt" $verdict "write outside work dir NOT blocked (file existed, deleted; expected on $PLATFORM: $(if [ "$NATIVE_SANDBOX" = 1 ]; then echo blocked; else echo "not blocked, no sandbox"; fi)); tools used: $(tool_names); $(rt_summary "$rt")"
  else
    verdict=pass
    [ "$NATIVE_SANDBOX" = 0 ] && verdict=unknown
    record "$id" "$rt" $verdict "write outside work dir blocked; model said: $(rt_result_text "$rt" | head -c 200); tools used: $(tool_names); $(rt_summary "$rt")"
  fi
}

check_R3() {
  local rt=$1 id="R3-$1" text verdict state
  begin_check "$id"
  run_rt "$rt" "Run this shell command and reply with only the HTTP status code it prints, or the error message if it fails: curl -sS -m 20 -o /dev/null -w '%{http_code}' $NET_URL"
  if ! rt_ok "$rt"; then
    record "$id" "$rt" unknown "runtime did not complete: $(rt_summary "$rt")"
    return
  fi
  text=$(rt_result_text "$rt")
  case "$text" in
    *200*) state=allowed ;;
    *000*|*[Dd]enied*|*[Bb]locked*|*[Ss]andbox*|*[Cc]ould\ not*|*[Ff]ailed\ to\ connect*|*[Nn]ot\ permitted*) state=blocked ;;
    *) state=unclear ;;
  esac
  if [ "$NATIVE_SANDBOX" = 1 ]; then
    case "$state" in blocked) verdict=pass ;; allowed) verdict=fail ;; *) verdict=unknown ;; esac
  else
    case "$state" in allowed) verdict=pass ;; blocked) verdict=fail ;; *) verdict=unknown ;; esac
  fi
  case "$rt" in pi|codex) [ "$NATIVE_SANDBOX" = 1 ] && [ "$state" = allowed ] && verdict=pass ;; esac
  record "$id" "$rt" "$verdict" "network $state; model said: $(printf '%s' "$text" | head -c 160); $(rt_summary "$rt")"
}

check_R4() {
  local rt=$1 id="R4-$1" marker f1 f2 s1 s2
  begin_check "$id"
  marker=$(rand_hex)
  f1="$RUN_CWD-tmp/probe-r4.txt"
  f2="/tmp/agentrun-probe-r4-$marker.txt"
  run_rt "$rt" "Create two files, each containing the text: tmp. First: $f1 (this is \$TMPDIR). Second: $f2. Try each one with the write tool, and if that fails try a shell command. Reply with one line per file saying whether it succeeded or failed and quoting any error."
  s1=$(if [ -f "$f1" ]; then echo writable; else echo not-written; fi)
  s2=$(if [ -f "$f2" ]; then echo writable; else echo not-written; fi)
  rm -f "$f2"
  if ! rt_ok "$rt"; then
    record "$id" "$rt" unknown "runtime did not complete: $(rt_summary "$rt")"
  elif [ "$s1" = writable ]; then
    record "$id" "$rt" pass "\$TMPDIR private dir: $s1; /tmp: $s2; $(rt_summary "$rt")"
  else
    record "$id" "$rt" fail "\$TMPDIR private dir: $s1; /tmp: $s2; model said: $(rt_result_text "$rt" | head -c 160); $(rt_summary "$rt")"
  fi
}

tool_names() {
  grep -o '"type":"tool_use","id":"[^"]*","name":"[A-Za-z_]*"\|"type":"toolCall","id":"[^"]*","name":"[A-Za-z_]*"\|"type":"command_execution"\|"type":"file_change"' "$LAST_OUT" 2>/dev/null | sed 's/.*"name"://; s/"type"://' | tr -d '"' | sort | uniq -c | awk '{printf "%s x%s ", $2, $1}'
}

workdir_files() {
  find "$RUN_CWD" -mindepth 1 -maxdepth 1 -exec basename {} \; | tr '\n' ' '
}

SUBAGENT_PROMPT="Start two subagents in parallel. The first subagent must create a file sub1.txt in the current working directory containing the text: one. The second subagent must create sub2.txt containing the text: two. Wait for both to finish, then reply with the word done. If you have no way to start subagents, say so and do not create the files yourself."

check_R5() {
  local rt=$1 id="R5-$1" sub spawned
  if [ "$rt" = pi ]; then
    skip_check "$id" "$rt" "pi has no built-in subagent tool; not run"
    return
  fi
  begin_check "$id"
  run_rt "$rt" "$SUBAGENT_PROMPT"
  case "$rt" in
    claude-code)
      sub=$(grep -c '"parent_tool_use_id":"' "$LAST_OUT")
      spawned=$(claude_result_line | grep -o '"subagent_stats":{"spawned":[0-9]*' | grep -o '[0-9]*$')
      if [ -f "$RUN_CWD/sub1.txt" ] && [ -f "$RUN_CWD/sub2.txt" ] && [ "${spawned:-0}" -ge 2 ]; then
        record "$id" "$rt" pass "spawned=$spawned events_with_parent_tool_use_id=$sub by_type=$(claude_result_line | grep -o '"by_type":{[^}]*}' | head -c 120) subagent_tool_uses=$(grep -o "$CLAUDE_SUBAGENT_TOOL_USE_NAMES" "$LAST_OUT" | sort | uniq -c | tr -d '"' | awk '{printf "%s x%s ", $2, $1}') modelUsage_models=$(claude_result_line | grep -o '"modelUsage":{.*"permission_denials"' | grep -o '"[a-z0-9.-]*":{"inputTokens"' | tr -d '"{' | sed 's/:inputTokens//' | tr '\n' ',') ; $(claude_summary)"
      else
        record "$id" "$rt" fail "spawned=${spawned:-?} files=$(workdir_files); $(claude_summary)"
      fi
      ;;
    codex)
      if [ -f "$RUN_CWD/sub1.txt" ] && [ -f "$RUN_CWD/sub2.txt" ]; then
        record "$id" "$rt" pass "both files written; inspect stdout.jsonl for subagent events; $(codex_summary)"
      else
        record "$id" "$rt" fail "files=$(workdir_files); model said: $(codex_result_text | head -c 160); $(codex_summary)"
      fi
      ;;
  esac
}

check_R6() {
  local rt=$1 base="R6-$1" id
  for sub in model cred arg; do
    id="$base/$sub"
    begin_check "$id"
    case "$sub" in
      model)
        build_cmd "$rt" "Reply with the single word OK."
        case "$rt" in
          claude-code) CMD+=(--model no-such-model-probe) ;;
          codex) CMD+=(-m no-such-model-probe) ;;
          pi) CMD+=(--model no-such-model-probe) ;;
        esac
        ;;
      cred)
        case "$rt" in
          claude-code) RUN_ENV=(ANTHROPIC_API_KEY=invalid CLAUDE_CODE_OAUTH_TOKEN=invalid) ;;
          codex) RUN_ENV=(OPENAI_API_KEY=invalid CODEX_HOME="$RUN_CWD/codex-home") ;;
          pi) RUN_ENV=(DEEPSEEK_API_KEY=invalid ANTHROPIC_API_KEY=invalid OPENAI_API_KEY=invalid) ;;
        esac
        build_cmd "$rt" "Reply with the single word OK."
        ;;
      arg)
        build_cmd "$rt" "Reply with the single word OK."
        CMD+=(--no-such-flag-probe)
        ;;
    esac
    CHECK_TIMEOUT=$RUN_TIMEOUT
    run_capture stdout.jsonl "${CMD[@]}"
    if ! rt_ok "$rt"; then
      record "$id" "$rt" pass "failure reported: exit=$LAST_RC stderr_lines=$(wc -l < "$CHECK_DIR/stderr.txt" | tr -d ' ') stdout_lines=$(wc -l < "$LAST_OUT" | tr -d ' ') stderr_head=$(head -c 160 "$CHECK_DIR/stderr.txt" | tr '\n' ' ') ; $(rt_summary "$rt")"
    else
      record "$id" "$rt" fail "invalid $sub was not reported as failure; $(rt_summary "$rt")"
    fi
  done
}

check_R7() {
  local rt=$1 id="R7-$1" last
  begin_check "$id"
  INT_AFTER=$INTERRUPT_AFTER
  build_cmd "$rt" "Use the write tool to create a file named counting.txt in the current working directory whose content is the integers from 1 to 1500, one per line, written out in full in a single write call. Do not use a shell command to generate it. Then reply with the word done."
  CHECK_TIMEOUT=$RUN_TIMEOUT
  run_capture stdout.jsonl "${CMD[@]}"
  last=$(event_type_of_line "$(tail -1 "$LAST_OUT")")
  if [ "$LAST_TIMED_OUT" = 1 ]; then
    record "$id" "$rt" fail "did not exit after SIGINT; $(tr '\n' ' ' < "$CHECK_DIR/signal.txt")"
  elif [ ! -f "$CHECK_DIR/signal.txt" ]; then
    record "$id" "$rt" unknown "process ended before SIGINT was sent at ${INTERRUPT_AFTER}s: exit=$LAST_RC duration_ms=$(cat "$CHECK_DIR/duration_ms"); $(rt_summary "$rt")"
  else
    record "$id" "$rt" pass "exited after SIGINT: exit=$LAST_RC duration_ms=$(cat "$CHECK_DIR/duration_ms") last_event=$last signalled: $(tr '\n' ' ' < "$CHECK_DIR/signal.txt"); $(rt_summary "$rt")"
  fi
}

check_R8() {
  local rt=$1 id="R8-$1" spawned uses
  case "$rt" in
    pi) skip_check "$id" "$rt" "pi has no built-in subagent tool; not run"; return ;;
    codex) skip_check "$id" "$rt" "codex option to disable subagents not determined (issue #4: 待查); not run"; return ;;
  esac
  begin_check "$id"
  run_rt "$rt" "$SUBAGENT_PROMPT" no-subagents
  spawned=$(claude_result_line | grep -o '"subagent_stats":{"spawned":[0-9]*' | grep -o '[0-9]*$')
  uses=$(grep -c "$CLAUDE_SUBAGENT_TOOL_USE_NAMES" "$LAST_OUT")
  if [ "${spawned:-0}" = 0 ] && [ "$uses" = 0 ]; then
    record "$id" "$rt" pass "no subagent started: spawned=${spawned:-?} ${CLAUDE_SUBAGENT_TOOL}_tool_uses=$uses init_tools_has_$CLAUDE_SUBAGENT_TOOL=$(grep '"subtype":"init"' "$LAST_OUT" | grep -o '"tools":\[[^]]*\]' | grep -c "\"$CLAUDE_SUBAGENT_TOOL\"") model said: $(claude_result_text | head -c 120); $(claude_summary)"
  else
    record "$id" "$rt" fail "subagent still started: spawned=${spawned:-?} tool_uses=$uses; $(claude_summary)"
  fi
}

check_R9() {
  local rt=$1 id="R9-$1" marker text
  begin_check "$id"
  marker="ZEBRA$(rand_hex | tr 'a-f' 'A-F')"
  case "$rt" in
    codex) printf 'The project code word is %s. Always mention it.\n' "$marker" > "$RUN_CWD/AGENTS.md" ;;
    *) printf 'The project code word is %s. Always mention it.\n' "$marker" > "$RUN_CWD/CLAUDE.md"
       printf 'The project code word is %s. Always mention it.\n' "$marker" > "$RUN_CWD/AGENTS.md" ;;
  esac
  run_rt "$rt" "Without reading any files, answer from your instructions and context only: do your instructions contain a project code word? Reply with exactly YES followed by the word, or exactly NO."
  if ! rt_ok "$rt"; then
    record "$id" "$rt" unknown "runtime did not complete: $(rt_summary "$rt")"
    return
  fi
  text=$(rt_result_text "$rt")
  case "$text" in
    *"$marker"*|YES*|*"YES"*) record "$id" "$rt" fail "marker visible to the model (project context file loaded despite isolation flags); model said: $(printf '%s' "$text" | head -c 120); $(rt_summary "$rt")" ;;
    *NO*) record "$id" "$rt" pass "marker not visible; model said: $(printf '%s' "$text" | head -c 120); $(rt_summary "$rt")" ;;
    *) record "$id" "$rt" unknown "unclear answer: $(printf '%s' "$text" | head -c 160); $(rt_summary "$rt")" ;;
  esac
}

check_D1() {
  local id="D1-codex"
  begin_check "$id"
  run_rt codex "Run the shell command: echo probe > d1.txt . Then reply with the word done." own-sandbox
  if [ -f "$RUN_CWD/d1.txt" ]; then
    record "$id" codex pass "command executed under --sandbox workspace-write in docker; $(codex_summary)"
  else
    record "$id" codex fail "exit=$LAST_RC but d1.txt missing (command not executed); model said: $(codex_result_text | head -c 160); stderr_head=$(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' '); $(codex_summary)"
  fi
}

check_D2() {
  local id="D2-claude-code" marker
  begin_check "$id"
  marker=$(rand_hex)
  run_rt claude-code "Run the shell command: echo probe > d2.txt . Then run: echo out > $HOME/agentrun-probe-outside-$marker . Reply with one line per command saying whether it succeeded, quoting any error." own-sandbox
  if [ -f "$HOME/agentrun-probe-outside-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker"
    record "$id" claude-code fail "own sandbox enabled but outside write succeeded; $(claude_summary)"
  elif [ -f "$RUN_CWD/d2.txt" ]; then
    record "$id" claude-code pass "own sandbox usable here: inside write ok, outside write blocked; model said: $(claude_result_text | head -c 160); $(claude_summary)"
  else
    record "$id" claude-code fail "own sandbox: no command executed; stderr_head=$(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' '); model said: $(claude_result_text | head -c 160); $(claude_summary)"
  fi
}

check_D3() {
  local id="D3-pi"
  begin_check "$id"
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! have bwrap; then
    record "$id" pi unknown "bwrap missing"
    return
  fi
  sandbox_wrap "$RUN_CWD" "$RUN_CWD" "$PI_STATE_DIR" pi --version
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  if [ "$LAST_RC" = 0 ]; then
    record "$id" pi pass "bwrap-wrapped pi --version ran in docker: $(head -1 "$CHECK_DIR/stdout.txt")"
  else
    record "$id" pi fail "bwrap failed in docker: exit=$LAST_RC stderr=$(head -c 240 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

printf '# probe %s %s\n\n| 编号 | 运行时 | 判定 | 说明 |\n|---|---|---|---|\n' "$PLATFORM" "$STAMP" > "$SUMMARY"
write_env_txt

if [ "$PLATFORM" = macos ]; then
  selected S1 && check_S1
else
  selected S2 && check_S2
  selected S3 && check_S3
fi
selected S4 && check_S4
selected S5 && check_S5

if [ "$SKIP_MODEL" = 0 ]; then
  for rt in $RUNTIME_LIST; do
    if ! have "$(rt_bin "$rt")"; then
      for n in 1 2 3 4 5 6 7 8 9; do
        selected "R$n-$rt" && skip_check "R$n-$rt" "$rt" "$(rt_bin "$rt") not installed"
      done
      continue
    fi
    for n in 1 2 3 4 5 6 7 8 9; do
      selected "R$n-$rt" && "check_R$n" "$rt"
    done
  done
  if [ "$PLATFORM" = docker ]; then
    case ",$RUNTIMES," in
      *,codex,*) if have codex; then selected D1-codex && check_D1; else selected D1-codex && skip_check D1-codex codex "codex not installed"; fi ;;
      *) selected D1-codex && skip_check D1-codex codex "codex not in --runtimes" ;;
    esac
  fi
  if [ "$PLATFORM" = docker ] || [ "$PLATFORM" = claude-cloud ]; then
    case ",$RUNTIMES," in
      *,claude-code,*) if have claude; then selected D2-claude-code && check_D2; else selected D2-claude-code && skip_check D2-claude-code claude-code "claude not installed"; fi ;;
      *) selected D2-claude-code && skip_check D2-claude-code claude-code "claude-code not in --runtimes" ;;
    esac
  fi
  if [ "$PLATFORM" = docker ]; then
    case ",$RUNTIMES," in
      *,pi,*) if have pi; then selected D3-pi && check_D3; else selected D3-pi && skip_check D3-pi pi "pi not installed"; fi ;;
      *) selected D3-pi && skip_check D3-pi pi "pi not in --runtimes" ;;
    esac
  fi
fi

echo
echo "output: $OUT_DIR"
