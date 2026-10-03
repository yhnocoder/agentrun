# shellcheck shell=bash disable=SC2034

SANDBOX_TIMEOUT=30
RUN_TIMEOUT=180
INTERRUPT_AFTER=10
NET_URL="https://api.github.com/"
CRED_PREFIXES="CLAUDE_ ANTHROPIC_ OPENAI_ CODEX_ DEEPSEEK_ AWS_ PI_ AGENTRUN_"
PI_STATE_DIR=${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}
CODEX_AUTH_WRITTEN="unset"
PI_AUTH_WRITTEN="unset"

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

restore_docker_auth() {
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
}

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

tool_names() {
  grep -o '"type":"tool_use","id":"[^"]*","name":"[A-Za-z_]*"\|"type":"toolCall","id":"[^"]*","name":"[A-Za-z_]*"\|"type":"command_execution"\|"type":"file_change"' "$LAST_OUT" 2>/dev/null | sed 's/.*"name"://; s/"type"://' | tr -d '"' | sort | uniq -c | awk '{printf "%s x%s ", $2, $1}'
}

workdir_files() {
  find "$RUN_CWD" -mindepth 1 -maxdepth 1 -exec basename {} \; | tr '\n' ' '
}
