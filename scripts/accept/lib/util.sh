# shellcheck shell=bash disable=SC2034

RUN_LIMIT_SECONDS=360
PROMPT_WAIT_SECONDS=120
EXIT_WAIT_SECONDS=30

FAIL_COUNT=0
SCENARIO=""
RT=""
REC=""
WORK=""
LAST_RC=""
LAST_MS=""
LAST_OUT=""
LAST_ERR=""
LAST_SIGNAL_MS=""
LAST_WATCHDOG=0

WANT_FORMAT=jsonl
WANT_TIMEOUT=300
WANT_SANDBOX=""
WANT_MODEL=default
WANT_DEBUG=0
WANT_SIGNAL=""
WANT_SIGNAL_DELAY=8

now_ms() {
  node -e 'process.stdout.write(String(Date.now()))'
}

rand_hex() {
  od -An -N4 -tx1 /dev/urandom | tr -d ' \n'
}

have() {
  command -v "$1" >/dev/null 2>&1
}

absolute_path() {
  local dir base
  dir=$(cd "$(dirname "$1")" 2>/dev/null && pwd -P) || return 1
  base=$(basename "$1")
  printf '%s/%s\n' "$dir" "$base"
}

version_line() {
  if have "$1"; then
    shift
    "$@" 2>&1 | head -n 1
  else
    printf 'not found\n'
  fi
}

reset_wants() {
  WANT_FORMAT=jsonl
  WANT_TIMEOUT=300
  WANT_SANDBOX=$SANDBOX
  WANT_MODEL=default
  WANT_DEBUG=0
  WANT_SIGNAL=""
  WANT_SIGNAL_DELAY=8
}

begin_scenario() {
  SCENARIO=$1
  RT=$2
  REC="$OUT_DIR/$SCENARIO-$RT"
  WORK="$PRIVATE_ROOT/$SCENARIO-$RT"
  mkdir -p "$REC" "$WORK"
  reset_wants
}

model_args() {
  case "$WANT_MODEL" in
    none) ;;
    default)
      case "$RT" in
        claude-code) printf -- '--model\n%s\n' "$CLAUDE_MODEL" ;;
        pi) printf -- '--model\n%s\n' "$PI_MODEL" ;;
      esac
      ;;
    *) printf -- '--model\n%s\n' "$WANT_MODEL" ;;
  esac
}

write_cmd_file() {
  local file=$1 cwd=$2 prompt=$3 arg
  shift 3
  {
    printf 'cwd: %s\n' "$cwd"
    printf 'cmd:'
    for arg in "$@"; do
      if [ "$arg" = "$prompt" ]; then
        printf ' <prompt>'
      else
        printf ' %q' "$arg"
      fi
    done
    printf '\n'
    printf 'prompt: %s\n' "$prompt"
  } > "$file"
}

wait_for_prompt_event() {
  local out=$1 pid=$2 i=0
  while [ "$i" -lt $((PROMPT_WAIT_SECONDS * 2)) ]; do
    if grep -q '"type":"prompt"' "$out" 2>/dev/null; then
      return 0
    fi
    kill -0 "$pid" 2>/dev/null || return 1
    sleep 0.5
    i=$((i + 1))
  done
  return 1
}

wait_for_exit() {
  local pid=$1 i=0
  while [ "$i" -lt $((EXIT_WAIT_SECONDS * 5)) ]; do
    kill -0 "$pid" 2>/dev/null || return 0
    sleep 0.2
    i=$((i + 1))
  done
  return 1
}

run_agentrun() {
  local rec=$1 cwd=$2 prompt=$3
  shift 3
  local cmd=() pid watchdog start end rc signal_at
  cmd=("$AGENTRUN" "$RT" --format "$WANT_FORMAT" --sandbox "$WANT_SANDBOX")
  if [ -n "$WANT_TIMEOUT" ]; then
    cmd+=(--timeout "$WANT_TIMEOUT")
  fi
  cmd+=(--raw "$rec/raw.jsonl")
  while IFS= read -r arg; do
    [ -n "$arg" ] && cmd+=("$arg")
  done <<EOT
$(model_args)
EOT
  if [ "$WANT_DEBUG" = 1 ]; then
    cmd+=(--debug)
  fi
  cmd+=(--prompt "$prompt" "$@")
  if [ "$WANT_FORMAT" = jsonl ]; then
    LAST_OUT="$rec/stdout.jsonl"
  else
    LAST_OUT="$rec/stdout.txt"
  fi
  LAST_ERR="$rec/stderr.txt"
  LAST_SIGNAL_MS=""
  LAST_WATCHDOG=0
  write_cmd_file "$rec/cmd.txt" "$cwd" "$prompt" "${cmd[@]}"
  start=$(now_ms)
  (cd "$cwd" && exec "${cmd[@]}") > "$LAST_OUT" 2> "$LAST_ERR" < /dev/null &
  pid=$!
  (
    trap 'kill "$sleeper" 2>/dev/null; exit 0' TERM
    sleep "$RUN_LIMIT_SECONDS" &
    sleeper=$!
    wait "$sleeper"
    touch "$rec/watchdog"
    kill -KILL "$pid" 2>/dev/null
  ) &
  watchdog=$!
  if [ -n "$WANT_SIGNAL" ]; then
    if wait_for_prompt_event "$LAST_OUT" "$pid"; then
      sleep "$WANT_SIGNAL_DELAY"
    fi
    signal_at=$(now_ms)
    kill "-$WANT_SIGNAL" "$pid" 2>/dev/null
    if wait_for_exit "$pid"; then
      LAST_SIGNAL_MS=$(( $(now_ms) - signal_at ))
    else
      LAST_SIGNAL_MS=$((EXIT_WAIT_SECONDS * 1000 + 1))
      kill -KILL "$pid" 2>/dev/null
    fi
  fi
  wait "$pid"
  rc=$?
  end=$(now_ms)
  kill "$watchdog" 2>/dev/null
  wait "$watchdog" 2>/dev/null
  if [ -f "$rec/watchdog" ]; then
    LAST_WATCHDOG=1
    rm -f "$rec/watchdog"
  fi
  LAST_RC=$rc
  LAST_MS=$((end - start))
  printf '%s\n' "$rc" > "$rec/exit_code"
  printf '%s\n' "$LAST_MS" > "$rec/duration_ms"
}

run_in_work() {
  run_agentrun "$REC" "$WORK" "$@"
}

JSONL_QUERY='
const fs = require("fs");
const file = process.argv[1];
const body = process.argv[2];
const args = process.argv.slice(3);
let text = "";
try { text = fs.readFileSync(file, "utf8"); } catch (e) {}
const events = [];
for (const line of text.split("\n")) {
  if (!line.trim()) continue;
  try { events.push(JSON.parse(line)); } catch (e) {}
}
const get = (obj, path) => path.split(".").reduce((v, k) => (v == null ? undefined : v[k]), obj);
const value = new Function("events", "args", "get", body)(events, args, get);
if (value === undefined || value === null) process.exit(0);
process.stdout.write(typeof value === "string" ? value : JSON.stringify(value));
'

jsonl_query() {
  node -e "$JSONL_QUERY" "$@" 2>/dev/null
}

ev_count() {
  jsonl_query "$LAST_OUT" 'return String(events.filter((e) => e.type === args[0]).length);' "$1"
}

ev_first() {
  jsonl_query "$LAST_OUT" 'const e = events.find((e) => e.type === args[0]); return e ? get(e, args[1]) : undefined;' "$1" "$2"
}

ev_last() {
  jsonl_query "$LAST_OUT" 'const list = events.filter((e) => e.type === args[0]); const e = list[list.length - 1]; return e ? get(e, args[1]) : undefined;' "$1" "$2"
}

ev_types() {
  jsonl_query "$LAST_OUT" 'return events.map((e) => e.type).join(" ");'
}

ev_line_count() {
  jsonl_query "$LAST_OUT" 'return String(events.length);'
}

ev_network() {
  jsonl_query "$LAST_OUT" 'const e = events.find((e) => e.type === "network" && e.host === args[0] && String(e.port) === args[1]); return e ? "allowed=" + e.allowed + " reason=" + e.reason : "";' "$1" "$2"
}

ev_tool_denied() {
  jsonl_query "$LAST_OUT" 'const e = events.find((e) => e.type === "tool" && e.name === args[0] && e.summary.indexOf(args[1]) >= 0); return e ? String(e.denied) : "";' "$1" "$2"
}

ev_argv_value() {
  jsonl_query "$LAST_OUT" 'const s = events.find((e) => e.type === "start"); if (!s) return ""; const i = s.argv.indexOf(args[0]); return i >= 0 && i + 1 < s.argv.length ? s.argv[i + 1] : "";' "$1"
}

ev_argv_joined() {
  jsonl_query "$LAST_OUT" 'const s = events.find((e) => e.type === "start"); return s ? s.argv.join(" ") : "";'
}

ev_subagent_end_status() {
  jsonl_query "$LAST_OUT" 'const s = events.find((e) => e.type === "subagent_start"); if (!s) return ""; const e = events.find((e) => e.type === "subagent_end" && e.id === s.id); return e ? e.status : "no subagent_end";'
}

end_status() {
  ev_last end status
}

end_detail() {
  ev_last end detail
}

stderr_has_line() {
  grep -q -x -F -- "$1" "$LAST_ERR" 2>/dev/null
}

stderr_line_starting() {
  grep -m 1 -- "^$1" "$LAST_ERR" 2>/dev/null | sed "s|^$1||"
}

stderr_tail() {
  tail -c 200 "$LAST_ERR" 2>/dev/null | tr '\n' ' '
}

first_line() {
  head -n 1 "$1" 2>/dev/null
}

last_line() {
  tail -n 1 "$1" 2>/dev/null
}

file_word() {
  tr -d ' \t\r\n' < "$1" 2>/dev/null
}

one_line() {
  printf '%s' "$1" | tr '\n\r' '  ' | sed 's/|/\\|/g'
}

record() {
  local verdict=$1 detail
  detail=$(one_line "$2")
  if [ "$LAST_WATCHDOG" = 1 ]; then
    verdict=fail
    detail="the script killed agentrun after ${RUN_LIMIT_SECONDS}s; $detail"
  fi
  [ "$verdict" = fail ] && FAIL_COUNT=$((FAIL_COUNT + 1))
  printf '%s: %s\n' "$verdict" "$detail" > "$REC/check.txt"
  printf '| %s | %s | %s | %s |\n' "$SCENARIO" "$RT" "$verdict" "$detail" >> "$SUMMARY"
  printf '== %s %s %s: %s\n' "$SCENARIO" "$RT" "$verdict" "$detail"
  LAST_WATCHDOG=0
}

pass() {
  record pass "$1"
}

fail() {
  record fail "$1"
}

skip() {
  record skip "$1"
}

run_summary() {
  printf 'exit %s, end.status %s' "$LAST_RC" "$(end_status)"
}

failure_summary() {
  local detail
  detail=$(end_detail)
  if [ -n "$detail" ]; then
    printf 'exit %s, end.status %s, detail: %s' "$LAST_RC" "$(end_status)" "$detail"
  else
    printf 'exit %s, end.status %s, stderr: %s' "$LAST_RC" "$(end_status)" "$(stderr_tail)"
  fi
}

finished_ok() {
  [ "$LAST_RC" = 0 ] && [ "$(end_status)" = finished ]
}
