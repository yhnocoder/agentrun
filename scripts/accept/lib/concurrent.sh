# shellcheck shell=bash disable=SC2034,SC2153

CONCURRENT_SCENARIOS="concurrent-same concurrent-same-cwd concurrent-network concurrent-interrupt concurrent-mixed"
CONCURRENT_TIMEOUT=30
CONCURRENT_PIDS=""
GROUP_DIRS=""
GATED_RUNTIMES=""

start_concurrent() {
  local rec=$1
  mkdir -p "$rec"
  (
    run_agentrun "$@"
    if [ "$LAST_WATCHDOG" = 1 ]; then
      : > "$rec/watchdog-fired"
    fi
  ) &
  CONCURRENT_PIDS="$CONCURRENT_PIDS $!"
}

wait_concurrent() {
  local pid
  for pid in $CONCURRENT_PIDS; do
    wait "$pid"
  done
  CONCURRENT_PIDS=""
}

load_run() {
  local rec=$1
  if [ -f "$rec/stdout.txt" ]; then
    LAST_OUT="$rec/stdout.txt"
  else
    LAST_OUT="$rec/stdout.jsonl"
  fi
  LAST_ERR="$rec/stderr.txt"
  LAST_RC=$(cat "$rec/exit_code" 2>/dev/null)
  LAST_MS=$(cat "$rec/duration_ms" 2>/dev/null)
  LAST_WATCHDOG=0
  if [ -f "$rec/watchdog-fired" ]; then
    LAST_WATCHDOG=1
  fi
}

note_run() {
  local rec=$1 verdict=$2 detail
  detail=$(one_line "$3")
  if [ -f "$rec/watchdog-fired" ]; then
    verdict=fail
    detail="the script killed agentrun after ${RUN_LIMIT_SECONDS}s; $detail"
  fi
  printf '%s: %s\n' "$verdict" "$detail" > "$rec/check.txt"
  [ "$verdict" = pass ]
}

processes_in_private_root() {
  local dir pid
  if [ -d /proc/self ]; then
    for dir in /proc/[0-9]*; do
      case "$(readlink "$dir/cwd" 2>/dev/null)" in
        "$PRIVATE_ROOT"/*) printf '%s\n' "${dir#/proc/}" ;;
      esac
    done
  else
    lsof -a -d cwd -Fn -w 2>/dev/null | awk -v root="$PRIVATE_ROOT/" '/^p/ { pid = substr($0, 2) } /^n/ && index(substr($0, 2), root) == 1 { print pid }'
  fi
}

begin_group() {
  GROUP_DIRS=$(tmp_dirs)
}

valid_json() {
  node -e 'JSON.parse(require("fs").readFileSync(process.argv[1], "utf8"))' "$1" 2>/dev/null
}

shared_file_note() {
  local name=$1 file=$2
  if [ ! -e "$file" ]; then
    printf '%s absent' "$name"
  elif valid_json "$file"; then
    printf '%s valid JSON' "$name"
  else
    printf '%s NOT valid JSON (%s)' "$name" "$file"
    return 1
  fi
}

shared_state() {
  local rt note ok=0 problems="" leftover pid args
  for rt in $1; do
    case "$rt" in
      claude-code) note=$(shared_file_note "claude.json" "${CLAUDE_CONFIG_DIR:-$HOME}/.claude.json") || problems="$problems; $note" ;;
      pi) note=$(shared_file_note "pi auth.json" "${PI_CODING_AGENT_DIR:-$HOME/.pi/agent}/auth.json") || problems="$problems; $note" ;;
      codex) note=$(shared_file_note "codex auth.json" "${CODEX_HOME:-$HOME/.codex}/auth.json") || problems="$problems; $note" ;;
    esac
    SHARED_NOTES="$SHARED_NOTES, $note"
    if ! basic_task_after "$rt"; then
      problems="$problems; basic-task with $rt afterwards: $BASIC_AFTER"
    fi
  done
  sleep 1
  if [ "$(tmp_dirs)" != "$GROUP_DIRS" ]; then
    problems="$problems; agentrun-* directories under $BASE_TMP changed: before [$GROUP_DIRS] after [$(tmp_dirs)]"
  fi
  leftover=""
  for pid in $(processes_in_private_root); do
    [ "$pid" = "$$" ] && continue
    args=$(ps -o args= -p "$pid" 2>/dev/null) || continue
    leftover="$leftover [$pid $args]"
    kill -KILL "$pid" 2>/dev/null
  done
  if [ -n "$leftover" ]; then
    problems="$problems; runtime processes left (killed):$leftover"
  fi
  SHARED_PROBLEMS=${problems#; }
  [ -z "$SHARED_PROBLEMS" ] || ok=1
  return "$ok"
}

basic_task_after() {
  local rt=$1 rec work saved_rt=$RT
  rec="$REC/after-basic-task-$rt"
  work="$WORK/after-$rt"
  mkdir -p "$rec" "$work"
  RT=$rt
  reset_wants
  run_agentrun "$rec" "$work" "$BASIC_PROMPT"
  RT=$saved_rt
  if finished_ok && [ "$(file_word "$work/accept.txt")" = hello ]; then
    printf 'pass: accept.txt written\n' > "$rec/check.txt"
    BASIC_AFTER="finished"
    return 0
  fi
  BASIC_AFTER=$(failure_summary)
  printf 'fail: %s\n' "$(one_line "$BASIC_AFTER")" > "$rec/check.txt"
  return 1
}

judge_group() {
  local failed=$1 summary=$2
  SHARED_NOTES=""
  if ! shared_state "$3"; then
    failed=1
    summary="$summary; shared state: $SHARED_PROBLEMS"
  fi
  summary="$summary; afterwards${SHARED_NOTES#,}"
  if [ "$failed" = 0 ]; then
    pass "$summary"
  else
    fail "$summary"
  fi
}

scenario_concurrent_same() {
  local i rec work failed=0 summary=""
  begin_group
  i=1
  while [ "$i" -le "$PARALLEL" ]; do
    mkdir -p "$WORK-$i"
    start_concurrent "$REC-$i" "$WORK-$i" "$BASIC_PROMPT"
    i=$((i + 1))
  done
  wait_concurrent
  i=1
  while [ "$i" -le "$PARALLEL" ]; do
    rec="$REC-$i"
    work="$WORK-$i"
    load_run "$rec"
    if finished_ok && [ "$(file_word "$work/accept.txt")" = hello ]; then
      note_run "$rec" pass "accept.txt written, ${LAST_MS}ms" || failed=1
      summary="$summary #$i finished ${LAST_MS}ms"
    else
      note_run "$rec" fail "$(failure_summary); accept.txt '$(file_word "$work/accept.txt")'"
      failed=1
      summary="$summary #$i $(failure_summary)"
    fi
    i=$((i + 1))
  done
  judge_group "$failed" "$PARALLEL runs:$summary" "$RT"
}

scenario_concurrent_same_cwd() {
  local name rec failed=0 summary=""
  begin_group
  for name in a b; do
    start_concurrent "$REC-$name" "$WORK" "Create a file named $name.txt containing the single word $name, then read it back. Do not touch any other file."
  done
  wait_concurrent
  for name in a b; do
    rec="$REC-$name"
    load_run "$rec"
    if finished_ok && [ "$(file_word "$WORK/$name.txt")" = "$name" ]; then
      note_run "$rec" pass "$name.txt written" || failed=1
      summary="$summary $name.txt: finished"
    else
      note_run "$rec" fail "$(failure_summary); $name.txt '$(file_word "$WORK/$name.txt")'"
      failed=1
      summary="$summary $name.txt: $(failure_summary), content '$(file_word "$WORK/$name.txt")'"
    fi
  done
  judge_group "$failed" "two runs in one directory:$summary" "$RT"
}

network_leaks() {
  jsonl_query "$LAST_OUT" 'return events.filter((e) => e.type === "network" && ((e.host === args[0] && !e.allowed) || (e.host === args[1] && e.allowed))).map((e) => e.host + ":" + e.port + " allowed=" + e.allowed).join(" ");' "$1" "$2"
}

scenario_concurrent_network() {
  local allowed other rec work a b good bad leaks failed=0 summary=""
  begin_group
  for allowed in api.github.com example.com; do
    mkdir -p "$WORK-$allowed"
    start_concurrent "$REC-$allowed" "$WORK-$allowed" "$SHELL_ONCE $(curl_to 10 https://api.github.com/ a.txt); $(curl_to 10 https://example.com/ b.txt) $NO_RETRY" --network custom --allow-host "$allowed"
  done
  wait_concurrent
  for allowed in api.github.com example.com; do
    rec="$REC-$allowed"
    work="$WORK-$allowed"
    load_run "$rec"
    a=$(file_word "$work/a.txt")
    b=$(file_word "$work/b.txt")
    if [ "$allowed" = api.github.com ]; then
      other=example.com
      good=$a
      bad=$b
    else
      other=api.github.com
      good=$b
      bad=$a
    fi
    leaks=""
    [ "$RT" != codex ] && leaks=$(network_leaks "$allowed" "$other")
    if [ "$(end_status)" != finished ]; then
      note_run "$rec" fail "$(failure_summary)"
      failed=1
      summary="$summary allow $allowed: $(failure_summary)"
    elif [ "$good" != 200 ] || [ -z "$bad" ] || [ "$bad" = 200 ]; then
      note_run "$rec" fail "a.txt (api.github.com) '${a:-missing}', b.txt (example.com) '${b:-missing}'"
      failed=1
      summary="$summary allow $allowed: a.txt '${a:-missing}' b.txt '${b:-missing}'"
    elif [ -n "$leaks" ]; then
      note_run "$rec" fail "network events that belong to the other run: $leaks"
      failed=1
      summary="$summary allow $allowed: foreign network events $leaks"
    else
      note_run "$rec" pass "a.txt $a, b.txt $b" || failed=1
      summary="$summary allow $allowed: a.txt $a b.txt $b"
    fi
  done
  [ "$RT" = codex ] && summary="$summary (codex: results only)"
  judge_group "$failed" "two runs:$summary" "$RT"
}

scenario_concurrent_interrupt() {
  local i rec failed=0 summary=""
  begin_group
  WANT_TIMEOUT=$CONCURRENT_TIMEOUT
  i=1
  while [ "$i" -le "$PARALLEL" ]; do
    mkdir -p "$WORK-$i"
    WANT_SIGNAL=""
    if [ "$i" = 1 ]; then
      WANT_SIGNAL=INT
    fi
    start_concurrent "$REC-$i" "$WORK-$i" "$SHELL_ONCE $LONG_COMMAND $NO_RETRY"
    i=$((i + 1))
  done
  reset_wants
  wait_concurrent
  i=1
  while [ "$i" -le "$PARALLEL" ]; do
    rec="$REC-$i"
    load_run "$rec"
    if [ "$i" = 1 ]; then
      if [ "$LAST_RC" = 130 ] && [ "$(end_status)" = interrupted ]; then
        note_run "$rec" pass "SIGINT: exit 130, interrupted" || failed=1
        summary="$summary #1 SIGINT exit 130"
      else
        note_run "$rec" fail "SIGINT: exit $LAST_RC (expected 130), end.status $(end_status)"
        failed=1
        summary="$summary #1 SIGINT exit $LAST_RC $(end_status)"
      fi
    elif [ "$LAST_RC" = 3 ] && [ "$(end_status)" = timeout ]; then
      note_run "$rec" pass "exit 3, timeout after ${LAST_MS}ms" || failed=1
      summary="$summary #$i timeout ${LAST_MS}ms"
    else
      note_run "$rec" fail "exit $LAST_RC (expected 3), end.status $(end_status)"
      failed=1
      summary="$summary #$i exit $LAST_RC $(end_status)"
    fi
    i=$((i + 1))
  done
  judge_group "$failed" "$PARALLEL runs, --timeout $CONCURRENT_TIMEOUT:$summary" "$RT"
}

scenario_concurrent_mixed() {
  local rt rec work failed=0 summary="" runtimes=""
  for rt in $RUNTIME_LIST; do
    in_list "$rt" "$GATED_RUNTIMES" || runtimes="$runtimes $rt"
  done
  if [ -z "$runtimes" ]; then
    RT=$(printf '%s' "$RUNTIMES")
    skip "every runtime is gated by the sandbox check"
    return
  fi
  begin_group
  for rt in $runtimes; do
    mkdir -p "$WORK-$rt"
    RT=$rt
    start_concurrent "$REC-$rt-1" "$WORK-$rt" "$BASIC_PROMPT"
  done
  wait_concurrent
  for rt in $runtimes; do
    RT=$rt
    rec="$REC-$rt-1"
    work="$WORK-$rt"
    load_run "$rec"
    if finished_ok; then
      note_run "$rec" pass "finished, accept.txt '$(file_word "$work/accept.txt")'" || failed=1
      summary="$summary $rt finished"
    else
      note_run "$rec" fail "$(failure_summary)"
      failed=1
      summary="$summary $rt $(failure_summary)"
    fi
  done
  RT=$(printf '%s' "${runtimes# }" | tr ' ' ',')
  judge_group "$failed" "one run each:$summary" "$runtimes"
}

run_concurrent_for_runtime() {
  local name
  [ -n "$SANDBOX_GATE" ] && GATED_RUNTIMES="$GATED_RUNTIMES $RT"
  for name in concurrent-same concurrent-same-cwd concurrent-network concurrent-interrupt; do
    in_list "$name" "$ONLY_LIST" || continue
    begin_scenario "$name" "$RT"
    if [ -n "$SANDBOX_GATE" ]; then
      skip "$SANDBOX_GATE"
    elif [ "$name" = concurrent-network ] && [ "$SANDBOX_RUNS" != 1 ]; then
      skip "sandbox not running"
    else
      "scenario_${name//-/_}"
    fi
  done
}

run_concurrent_mixed() {
  in_list concurrent-mixed "$ONLY_LIST" || return 0
  SCENARIO=concurrent-mixed
  REC="$OUT_DIR/$SCENARIO"
  WORK="$PRIVATE_ROOT/$SCENARIO"
  mkdir -p "$REC" "$WORK"
  reset_wants
  scenario_concurrent_mixed
}
