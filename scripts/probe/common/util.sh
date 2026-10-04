# shellcheck shell=bash disable=SC2034,SC2030,SC2031

SANDBOX_TIMEOUT=30
RUN_TIMEOUT=180
PREFLIGHT_TIMEOUT=5
KILL_GRACE=5
INTERRUPT_AFTER=10
NET_URL="https://api.github.com/"
NET_HOST="api.github.com"
DENIED_HOST="example.com"
ENV_NAME_PREFIXES="CLAUDE_ ANTHROPIC_ OPENAI_ CODEX_ DEEPSEEK_ AWS_ PI_ AGENTRUN_"
CODEX_AUTH_WRITTEN="unset"
PI_AUTH_WRITTEN="unset"

CHECK_ID=""
CHECK_DIR=""
RUN_CWD=""
SESSION_TMP=""
CHECK_TIMEOUT=$RUN_TIMEOUT
RUN_ENV=()
DROP_ENV=()
STDIN_FILE=/dev/null
INT_AFTER=""
INT_SIGNAL=INT
LAST_RC=""
LAST_TIMED_OUT=0
LAST_OUT=""

now_ms() {
  perl -MTime::HiRes -e 'printf "%d", Time::HiRes::time()*1000'
}

rand_hex() {
  od -An -N4 -tx1 /dev/urandom | tr -d ' \n'
}

new_uuid() {
  local h
  h=$(od -An -N16 -tx1 /dev/urandom | tr -d ' \n')
  printf '%s-%s-4%s-%s%s-%s\n' "${h:0:8}" "${h:8:4}" "${h:13:3}" "$(printf '%x' $(( (0x${h:16:1} & 3) | 8 )))" "${h:17:3}" "${h:20:12}"
}

have() {
  command -v "$1" >/dev/null 2>&1
}

real_dir() {
  (cd "$1" 2>/dev/null && pwd -P)
}

real_file() {
  perl -MCwd=realpath -e 'print realpath($ARGV[0])' "$1"
}

env_names() {
  awk 'BEGIN { for (k in ENVIRON) print k }' | sort
}

env_names_with_prefix() {
  local p
  env_names | while read -r v; do
    for p in "$@"; do
      case "$v" in "$p"*) echo "$v" ;; esac
    done
  done
}

make_private_dirs() {
  (umask 077 && mkdir -p "$1")
}

write_auth_file() {
  local value=$1 dir=$2 tmp
  make_private_dirs "$dir" || return 1
  tmp="$dir/auth.json.probe-$(rand_hex)"
  (umask 077 && printf '%s' "$value" > "$tmp") || return 1
  mv -f "$tmp" "$dir/auth.json"
}

write_credentials() {
  if [ -n "${AGENTRUN_CODEX_AUTH:-}" ]; then
    if write_auth_file "$AGENTRUN_CODEX_AUTH" "${CODEX_HOME:-$HOME/.codex}"; then
      CODEX_AUTH_WRITTEN="written to ${CODEX_HOME:-$HOME/.codex}/auth.json"
    else
      CODEX_AUTH_WRITTEN="write failed"
    fi
  fi
  if [ -n "${AGENTRUN_PI_AUTH:-}" ]; then
    if write_auth_file "$AGENTRUN_PI_AUTH" "$PI_STATE_DIR"; then
      PI_AUTH_WRITTEN="written to $PI_STATE_DIR/auth.json"
    else
      PI_AUTH_WRITTEN="write failed"
    fi
  fi
}

selected() {
  local name=$1 rt=${2:-} pat
  [ -z "$ONLY" ] && return 0
  for pat in $(printf '%s' "$ONLY" | tr ',' ' '); do
    [ "$pat" = "$name" ] && return 0
    [ -n "$rt" ] && [ "$pat" = "$name-$rt" ] && return 0
  done
  return 1
}

begin_check() {
  CHECK_ID=$1
  CHECK_DIR="$OUT_DIR/$CHECK_ID"
  mkdir -p "$CHECK_DIR"
  RUN_CWD="$PRIVATE_ROOT/$CHECK_ID"
  mkdir -p "$RUN_CWD"
  SESSION_TMP="$BASE_TMP/agentrun-$(rand_hex)"
  mkdir -m 0700 "$SESSION_TMP"
  RUN_ENV=("TMPDIR=$SESSION_TMP" "TMP=$SESSION_TMP" "TEMP=$SESSION_TMP")
  DROP_ENV=()
  while read -r v; do
    [ -n "$v" ] && DROP_ENV+=("$v")
  done <<EOF
$(env_names_with_prefix AGENTRUN_)
EOF
  STDIN_FILE=/dev/null
  INT_AFTER=""
  INT_SIGNAL=INT
  CHECK_TIMEOUT=$RUN_TIMEOUT
  LAST_RC=""
  LAST_TIMED_OUT=0
  LAST_OUT=""
  echo "== $CHECK_ID"
}

end_check() {
  stop_proxy
  [ -n "$SESSION_TMP" ] && rm -rf "$SESSION_TMP"
  SESSION_TMP=""
}

write_cmd() {
  local f="$CHECK_DIR/cmd.txt" e
  {
    printf 'cwd: %s\n' "$RUN_CWD"
    printf 'session tmp: %s\n' "$SESSION_TMP"
    printf 'stdin: %s\n' "$STDIN_FILE"
    for e in ${DROP_ENV[@]+"${DROP_ENV[@]}"}; do
      printf 'env removed: %s\n' "$e"
    done
    for e in ${RUN_ENV[@]+"${RUN_ENV[@]}"}; do
      printf 'env set: %s\n' "${e%%=*}"
    done
    printf 'cmd:'
    for e in "$@"; do printf ' %q' "$e"; done
    printf '\n'
  } > "$f"
}

kill_group() {
  kill "-$1" -- "-$2" 2>/dev/null
}

run_capture() {
  local out=$1; shift
  local start pid wd ip rc end unset_args=() e
  LAST_OUT="$CHECK_DIR/$out"
  write_cmd "$@"
  rm -f "$CHECK_DIR/timed_out"
  for e in ${DROP_ENV[@]+"${DROP_ENV[@]}"}; do
    unset_args+=(-u "$e")
  done
  start=$(now_ms)
  (
    cd "$RUN_CWD" && exec env ${unset_args[@]+"${unset_args[@]}"} ${RUN_ENV[@]+"${RUN_ENV[@]}"} \
      perl -e 'setpgrp(0, 0) or die "setpgrp: $!"; exec @ARGV or die "exec: $!"' -- "$@"
  ) >"$LAST_OUT" 2>"$CHECK_DIR/stderr.txt" <"$STDIN_FILE" &
  pid=$!
  (
    trap 'kill $s 2>/dev/null; exit 0' TERM
    sleep "$CHECK_TIMEOUT" & s=$!
    wait $s
    touch "$CHECK_DIR/timed_out"
    kill_group TERM "$pid"
    sleep "$KILL_GRACE" & s=$!
    wait $s
    kill_group KILL "$pid"
  ) &
  wd=$!
  ip=""
  if [ -n "$INT_AFTER" ]; then
    (
      trap 'kill $s 2>/dev/null; exit 0' TERM
      sleep "$INT_AFTER" & s=$!
      wait $s
      {
        printf 'sent SIG%s to process group %s at %sms after start\n' "$INT_SIGNAL" "$pid" "$(( $(now_ms) - start ))"
        printf 'stdout lines before signal: %s\n' "$(wc -l < "$LAST_OUT" | tr -d ' ')"
        printf 'prompt replays before signal: %s\n' "$(grep -c '"isReplay":true' "$LAST_OUT")"
        printf 'processes in group:'
        pgrep -l -g "$pid" 2>/dev/null | tr '\n' ';'
        printf '\n'
      } > "$CHECK_DIR/signal.txt"
      kill_group "$INT_SIGNAL" "$pid"
    ) &
    ip=$!
  fi
  wait "$pid"
  rc=$?
  end=$(now_ms)
  kill_group KILL "$pid"
  kill "$wd" 2>/dev/null
  [ -n "$ip" ] && kill "$ip" 2>/dev/null
  wait "$wd" 2>/dev/null
  [ -n "$ip" ] && wait "$ip" 2>/dev/null
  if [ -f "$CHECK_DIR/timed_out" ]; then
    LAST_TIMED_OUT=1
    rm -f "$CHECK_DIR/timed_out"
  fi
  LAST_RC=$rc
  printf '%s\n' "$rc" > "$CHECK_DIR/exit_code"
  printf '%s\n' $((end - start)) > "$CHECK_DIR/duration_ms"
}

record() {
  local name=$1 rt=$2 verdict=$3 detail=$4
  if [ "$LAST_TIMED_OUT" = 1 ]; then
    verdict=timeout
    detail="exceeded ${CHECK_TIMEOUT}s; $detail"
  fi
  detail=$(printf '%s' "$detail" | tr '\n' ' ' | sed 's/|/\\|/g')
  printf '%s\n%s\n' "$verdict" "$detail" > "$CHECK_DIR/check.txt"
  printf '| %s | %s | %s | %s |\n' "$name" "$rt" "$verdict" "$detail" >> "$SUMMARY"
  echo "   $verdict: $detail"
}

skip_check() {
  local id=$1 name=$2 rt=$3 detail=$4
  CHECK_ID=$id
  CHECK_DIR="$OUT_DIR/$id"
  mkdir -p "$CHECK_DIR"
  LAST_TIMED_OUT=0
  echo "== $id"
  record "$name" "$rt" unknown "$detail"
}

stderr_head() {
  head -c "${1:-200}" "$CHECK_DIR/stderr.txt" 2>/dev/null | tr '\n' ' '
}

jsonl_query() {
  local file=$1 js=$2
  node -e '
const fs = require("fs");
const lines = fs.readFileSync(process.argv[1], "utf8").split("\n").filter(Boolean).map((l) => {
  try { return JSON.parse(l); } catch (e) { return { __raw: l }; }
});
const v = new Function("lines", process.argv[2])(lines);
if (v !== undefined && v !== null) process.stdout.write(typeof v === "string" ? v : JSON.stringify(v));
' "$file" "$js" 2>/dev/null
}

event_types() {
  jsonl_query "$LAST_OUT" '
const c = {};
for (const l of lines) {
  const k = l.__raw !== undefined ? "unparsed" : l.type + (l.subtype ? "/" + l.subtype : "");
  c[k] = (c[k] || 0) + 1;
}
return Object.entries(c).map(([k, n]) => k + " x" + n).join(" ");
'
}

workdir_files() {
  find "$RUN_CWD" -mindepth 1 -maxdepth 1 -exec basename {} \; | tr '\n' ' '
}

write_env_txt() {
  local f="$OUT_DIR/env.txt" rt p v
  {
    echo "sandbox: $SANDBOX ($SANDBOX_KIND)"
    echo "label: $LABEL"
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
    echo "bash: $BASH_VERSION"
    echo "TMPDIR: [${TMPDIR:-}]"
    echo "private root: $PRIVATE_ROOT"
    echo "container hints:"
    [ -f /.dockerenv ] && echo "  /.dockerenv exists"
    [ -r /proc/1/cgroup ] && echo "  /proc/1/cgroup: $(tr '\n' ' ' < /proc/1/cgroup)"
    [ -n "${container:-}" ] && echo "  \$container is set"
    [ -n "${CLAUDE_CODE_REMOTE:-}" ] && echo "  CLAUDE_CODE_REMOTE is set"
    [ -n "${CODEX_SANDBOX:-}" ] && echo "  CODEX_SANDBOX is set"
    echo "userns (unshare -U -r true): $(if have unshare; then if unshare -U -r true 2>/dev/null; then echo ok; else echo failed; fi; else echo "unshare missing"; fi)"
    for p in bwrap socat sandbox-exec curl node perl; do
      if [ "$p" = sandbox-exec ]; then
        if [ -x /usr/bin/sandbox-exec ]; then echo "sandbox-exec: /usr/bin/sandbox-exec (present)"; else echo "sandbox-exec: missing"; fi
        continue
      fi
      if have "$p"; then
        case "$p" in
          bwrap) v=$(bwrap --version 2>&1 | head -1) ;;
          socat) v=$(socat -V 2>&1 | head -1) ;;
          curl) v=$(curl --version 2>&1 | head -1) ;;
          node) v=$(node --version 2>&1) ;;
          perl) v=$(perl -e 'print $^V') ;;
        esac
        echo "$p: $(command -v "$p") ($v)"
      else
        echo "$p: missing"
      fi
    done
    echo "HTTPS_PROXY: $(if [ -n "${HTTPS_PROXY:-}" ]; then echo set; else echo unset; fi)"
    echo "env names (prefixes: $ENV_NAME_PREFIXES):"
    # shellcheck disable=SC2086
    env_names_with_prefix $ENV_NAME_PREFIXES | sed 's/^/  /'
    echo "AGENTRUN_CODEX_AUTH: $CODEX_AUTH_WRITTEN"
    echo "AGENTRUN_PI_AUTH: $PI_AUTH_WRITTEN"
    echo "pi state dir: $PI_STATE_DIR $(if [ -d "$PI_STATE_DIR" ]; then echo exists; else echo missing; fi)"
    echo "pi auth.json target: ${PI_AUTH_TARGET:-none}"
    echo "codex auth.json target: ${CODEX_AUTH_TARGET:-none}"
    echo "pi default provider: ${PI_PROVIDER:-unknown} -> host ${PI_MODEL_HOST:-unknown}"
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
