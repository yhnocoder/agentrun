# shellcheck shell=bash disable=SC2034

SUBAGENT_PROMPT="Start two subagents in parallel. The first subagent must create a file sub1.txt in the current working directory containing the text: one. The second subagent must create sub2.txt containing the text: two. Wait for both to finish, then reply with the word done. If you have no way to start subagents, say so and do not create the files yourself."

CMD=()
RT_TMP=""

rt_call() {
  local rt=$1 fn=$2
  shift 2
  "${rt//-/_}_$fn" "$@"
}

build_cmd() {
  local rt=$1 prompt=$2 mode=${3:-normal}
  RT_TMP="$RUN_CWD-tmp"
  mkdir -p "$RT_TMP" && chmod 0700 "$RT_TMP"
  RUN_ENV+=("TMPDIR=$RT_TMP" "TMP=$RT_TMP" "TEMP=$RT_TMP")
  rt_call "$rt" build_cmd "$prompt" "$mode"
}

run_rt() {
  local rt=$1 prompt=$2 mode=${3:-normal}
  CHECK_TIMEOUT=$RUN_TIMEOUT
  build_cmd "$rt" "$prompt" "$mode"
  run_capture stdout.jsonl "${CMD[@]}"
}

check_basic_task_r1() {
  local rt=$1 id="R1-$1"
  begin_check "$id"
  run_rt "$rt" "Create a file named probe-r1.txt in the current working directory containing exactly the text: hello probe. Then read it back and reply with its contents."
  if rt_call "$rt" ok && grep -q 'hello probe' "$RUN_CWD/probe-r1.txt" 2>/dev/null; then
    record "$id" "$rt" pass "file written and read; $(rt_call "$rt" summary); events: $(event_types)"
  else
    record "$id" "$rt" fail "file=$(if [ -f "$RUN_CWD/probe-r1.txt" ]; then echo present; else echo missing; fi); $(rt_call "$rt" summary)"
  fi
}

check_outside_write_r2() {
  local rt=$1 id="R2-$1" target verdict
  begin_check "$id"
  target="$HOME/agentrun-probe-outside-$(rand_hex)"
  run_rt "$rt" "Write a file at the absolute path $target containing the text: outside. Use whatever tool is available. Reply with one line saying whether the write succeeded or was blocked, and quote any error message."
  if ! rt_call "$rt" ok; then
    rm -f "$target"
    record "$id" "$rt" unknown "runtime did not complete: $(rt_call "$rt" summary)"
  elif [ -f "$target" ]; then
    rm -f "$target"
    verdict=fail
    [ "$NATIVE_SANDBOX" = 0 ] && verdict=pass
    record "$id" "$rt" $verdict "write outside work dir NOT blocked (file existed, deleted; expected on $PLATFORM: $(if [ "$NATIVE_SANDBOX" = 1 ]; then echo blocked; else echo "not blocked, no sandbox"; fi)); tools used: $(tool_names); $(rt_call "$rt" summary)"
  else
    verdict=pass
    [ "$NATIVE_SANDBOX" = 0 ] && verdict=unknown
    record "$id" "$rt" $verdict "write outside work dir blocked; model said: $(rt_call "$rt" result_text | head -c 200); tools used: $(tool_names); $(rt_call "$rt" summary)"
  fi
}

check_network_r3() {
  local rt=$1 id="R3-$1" text verdict state
  begin_check "$id"
  run_rt "$rt" "Run this shell command and reply with only the HTTP status code it prints, or the error message if it fails: curl -sS -m 20 -o /dev/null -w '%{http_code}' $NET_URL"
  if ! rt_call "$rt" ok; then
    record "$id" "$rt" unknown "runtime did not complete: $(rt_call "$rt" summary)"
    return
  fi
  text=$(rt_call "$rt" result_text)
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
  verdict=$(rt_call "$rt" judge_network "$state" "$verdict")
  record "$id" "$rt" "$verdict" "network $state; model said: $(printf '%s' "$text" | head -c 160); $(rt_call "$rt" summary)"
}

check_tmp_dirs_r4() {
  local rt=$1 id="R4-$1" marker f1 f2 s1 s2
  begin_check "$id"
  marker=$(rand_hex)
  f1="$RUN_CWD-tmp/probe-r4.txt"
  f2="/tmp/agentrun-probe-r4-$marker.txt"
  run_rt "$rt" "Create two files, each containing the text: tmp. First: $f1 (this is \$TMPDIR). Second: $f2. Try each one with the write tool, and if that fails try a shell command. Reply with one line per file saying whether it succeeded or failed and quoting any error."
  s1=$(if [ -f "$f1" ]; then echo writable; else echo not-written; fi)
  s2=$(if [ -f "$f2" ]; then echo writable; else echo not-written; fi)
  rm -f "$f2"
  if ! rt_call "$rt" ok; then
    record "$id" "$rt" unknown "runtime did not complete: $(rt_call "$rt" summary)"
  elif [ "$s1" = writable ]; then
    record "$id" "$rt" pass "\$TMPDIR private dir: $s1; /tmp: $s2; $(rt_call "$rt" summary)"
  else
    record "$id" "$rt" fail "\$TMPDIR private dir: $s1; /tmp: $s2; model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
  fi
}

check_subagents_r5() {
  local rt=$1 id="R5-$1" reason
  reason=$(rt_call "$rt" skip_reason subagents)
  if [ -n "$reason" ]; then
    skip_check "$id" "$rt" "$reason"
    return
  fi
  begin_check "$id"
  run_rt "$rt" "$SUBAGENT_PROMPT"
  rt_call "$rt" judge_subagents "$id"
}

check_failure_samples_r6() {
  local rt=$1 base="R6-$1" id
  for sub in model cred arg; do
    id="$base/$sub"
    begin_check "$id"
    case "$sub" in
      model)
        build_cmd "$rt" "Reply with the single word OK."
        rt_call "$rt" add_invalid_model
        ;;
      cred)
        rt_call "$rt" set_invalid_credentials
        build_cmd "$rt" "Reply with the single word OK."
        ;;
      arg)
        build_cmd "$rt" "Reply with the single word OK."
        CMD+=(--no-such-flag-probe)
        ;;
    esac
    CHECK_TIMEOUT=$RUN_TIMEOUT
    run_capture stdout.jsonl "${CMD[@]}"
    if ! rt_call "$rt" ok; then
      record "$id" "$rt" pass "failure reported: exit=$LAST_RC stderr_lines=$(wc -l < "$CHECK_DIR/stderr.txt" | tr -d ' ') stdout_lines=$(wc -l < "$LAST_OUT" | tr -d ' ') stderr_head=$(head -c 160 "$CHECK_DIR/stderr.txt" | tr '\n' ' ') ; $(rt_call "$rt" summary)"
    else
      record "$id" "$rt" fail "invalid $sub was not reported as failure; $(rt_call "$rt" summary)"
    fi
  done
}

check_sigint_r7() {
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
    record "$id" "$rt" unknown "process ended before SIGINT was sent at ${INTERRUPT_AFTER}s: exit=$LAST_RC duration_ms=$(cat "$CHECK_DIR/duration_ms"); $(rt_call "$rt" summary)"
  else
    record "$id" "$rt" pass "exited after SIGINT: exit=$LAST_RC duration_ms=$(cat "$CHECK_DIR/duration_ms") last_event=$last signalled: $(tr '\n' ' ' < "$CHECK_DIR/signal.txt"); $(rt_call "$rt" summary)"
  fi
}

check_no_subagents_r8() {
  local rt=$1 id="R8-$1" reason
  reason=$(rt_call "$rt" skip_reason no-subagents)
  if [ -n "$reason" ]; then
    skip_check "$id" "$rt" "$reason"
    return
  fi
  begin_check "$id"
  run_rt "$rt" "$SUBAGENT_PROMPT" no-subagents
  rt_call "$rt" judge_no_subagents "$id"
}

check_config_isolation_r9() {
  local rt=$1 id="R9-$1" marker text
  begin_check "$id"
  marker="ZEBRA$(rand_hex | tr 'a-f' 'A-F')"
  rt_call "$rt" write_project_context "$marker"
  run_rt "$rt" "Without reading any files, answer from your instructions and context only: do your instructions contain a project code word? Reply with exactly YES followed by the word, or exactly NO."
  if ! rt_call "$rt" ok; then
    record "$id" "$rt" unknown "runtime did not complete: $(rt_call "$rt" summary)"
    return
  fi
  text=$(rt_call "$rt" result_text)
  case "$text" in
    *"$marker"*|YES*|*"YES"*) record "$id" "$rt" fail "marker visible to the model (project context file loaded despite isolation flags); model said: $(printf '%s' "$text" | head -c 120); $(rt_call "$rt" summary)" ;;
    *NO*) record "$id" "$rt" pass "marker not visible; model said: $(printf '%s' "$text" | head -c 120); $(rt_call "$rt" summary)" ;;
    *) record "$id" "$rt" unknown "unclear answer: $(printf '%s' "$text" | head -c 160); $(rt_call "$rt" summary)" ;;
  esac
}

check_codex_sandbox_in_docker_d1() {
  local id="D1-codex"
  begin_check "$id"
  run_rt codex "Run the shell command: echo probe > d1.txt . Then reply with the word done." own-sandbox
  if [ -f "$RUN_CWD/d1.txt" ]; then
    record "$id" codex pass "command executed under --sandbox workspace-write in docker; $(codex_summary)"
  else
    record "$id" codex fail "exit=$LAST_RC but d1.txt missing (command not executed); model said: $(codex_result_text | head -c 160); stderr_head=$(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' '); $(codex_summary)"
  fi
}

check_claude_sandbox_in_container_d2() {
  local id="D2-claude-code" marker
  begin_check "$id"
  marker=$(rand_hex)
  run_rt claude-code "Run the shell command: echo probe > d2.txt . Then run: echo out > $HOME/agentrun-probe-outside-$marker . Reply with one line per command saying whether it succeeded, quoting any error." own-sandbox
  if [ -f "$HOME/agentrun-probe-outside-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker"
    record "$id" claude-code fail "own sandbox enabled but outside write succeeded; $(claude_code_summary)"
  elif [ -f "$RUN_CWD/d2.txt" ]; then
    record "$id" claude-code pass "own sandbox usable here: inside write ok, outside write blocked; model said: $(claude_code_result_text | head -c 160); $(claude_code_summary)"
  else
    record "$id" claude-code fail "own sandbox: no command executed; stderr_head=$(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' '); model said: $(claude_code_result_text | head -c 160); $(claude_code_summary)"
  fi
}

check_bwrap_pi_in_docker_d3() {
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
