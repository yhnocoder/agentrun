# shellcheck shell=bash disable=SC2034

MODEL_CHECKS="basic-task outside-write write-tool-outside network tmp-dirs claude-allow-write claude-fail-if-unavailable subagents no-subagents failure-samples sigint config-isolation prompt-multiline prompt-long codex-home codex-proxy"

SUBAGENT_PROMPT="Start two subagents in parallel. The first subagent must create a file sub1.txt in the current working directory containing the text: one. The second subagent must create sub2.txt containing the text: two. Wait for both to finish, then reply with the word done. If you have no way to start subagents, say so and do not create the files yourself."

CMD=()

rt_call() {
  local rt=$1 fn=$2
  shift 2
  "${rt//-/_}_$fn" "$@"
}

check_applies() {
  local name=$1 rt=$2
  case "$name" in
    write-tool-outside|prompt-multiline|prompt-long) [ "$rt" = claude-code ] ;;
    claude-allow-write|claude-fail-if-unavailable) [ "$rt" = claude-code ] && [ "$SANDBOX" = on ] ;;
    subagents|no-subagents) [ "$rt" != pi ] ;;
    codex-home) [ "$rt" = codex ] ;;
    codex-proxy) [ "$rt" = codex ] && [ "$SANDBOX" = on ] ;;
    *) return 0 ;;
  esac
}

build_cmd() {
  local rt=$1 prompt=$2 mode=${3:-normal}
  local names
  names=$(rt_call "$rt" drop_env_names)
  for v in $names; do
    DROP_ENV+=("$v")
  done
  rt_call "$rt" build_cmd "$prompt" "$mode"
}

run_rt() {
  local rt=$1 prompt=$2 mode=${3:-normal}
  CHECK_TIMEOUT=$RUN_TIMEOUT
  if ! build_cmd "$rt" "$prompt" "$mode"; then
    LAST_OUT="$CHECK_DIR/stdout.jsonl"
    : > "$LAST_OUT"
    echo "probe: could not build the command (proxy start failed?)" > "$CHECK_DIR/stderr.txt"
    LAST_RC=1
    printf '1\n' > "$CHECK_DIR/exit_code"
    printf '0\n' > "$CHECK_DIR/duration_ms"
    return 1
  fi
  run_capture stdout.jsonl "${CMD[@]}"
  stop_proxy
}

not_completed() {
  local name=$1 rt=$2
  record "$name" "$rt" unknown "runtime did not complete: $(rt_call "$rt" summary)"
}

check_basic_task() {
  local rt=$1 name=basic-task
  begin_check "$name-$rt"
  run_rt "$rt" "Create a file named probe.txt in the current working directory containing exactly the text: hello probe. Then read it back and reply with its contents."
  if rt_call "$rt" ok && grep -q 'hello probe' "$RUN_CWD/probe.txt" 2>/dev/null; then
    record "$name" "$rt" pass "file written and read back; $(rt_call "$rt" summary); events: $(event_types)"
  else
    record "$name" "$rt" fail "file=$(if [ -f "$RUN_CWD/probe.txt" ]; then echo present; else echo missing; fi); $(rt_call "$rt" summary)"
  fi
  end_check
}

check_outside_write() {
  local rt=$1 name=outside-write target state
  begin_check "$name-$rt"
  target="$HOME/agentrun-probe-outside-$(rand_hex)"
  run_rt "$rt" "Write a file at the absolute path $target containing the text: outside. Use whatever tool is available: a file writing tool or a shell command. Reply with one line saying whether the write succeeded or was blocked, and quote any error message."
  state=blocked
  [ -f "$target" ] && state=written
  rm -f "$target"
  if ! rt_call "$rt" ok; then
    not_completed "$name" "$rt"
  elif [ "$SANDBOX" = on ]; then
    if [ "$state" = blocked ]; then
      record "$name" "$rt" pass "write outside cwd blocked (file absent); model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
    else
      record "$name" "$rt" fail "write outside cwd succeeded (file existed, deleted); model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
    fi
  else
    if [ "$state" = written ]; then
      record "$name" "$rt" pass "write outside cwd succeeded without sandbox (file deleted); $(rt_call "$rt" summary)"
    else
      record "$name" "$rt" fail "write outside cwd did not happen without sandbox; model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
    fi
  fi
  end_check
}

check_write_tool_outside() {
  local rt=$1 name=write-tool-outside marker home_file tmp_file h t
  begin_check "$name-$rt"
  marker=$(rand_hex)
  home_file="$HOME/agentrun-probe-outside-$marker"
  tmp_file="/tmp/agentrun-probe-outside-$marker"
  run_rt "$rt" "Use only the Write tool, never a shell command. Write the text: outside into two files: $home_file and $tmp_file. Reply with one line per file saying whether the write succeeded or was blocked, quoting any error." write-only
  h=blocked; t=blocked
  [ -f "$home_file" ] && h=written
  [ -f "$tmp_file" ] && t=written
  rm -f "$home_file" "$tmp_file"
  if ! rt_call "$rt" ok; then
    not_completed "$name" "$rt"
  elif [ "$SANDBOX" = on ]; then
    if [ "$h" = blocked ] && [ "$t" = blocked ]; then
      record "$name" "$rt" pass "Write tool: \$HOME=$h /tmp=$t; $(rt_call "$rt" summary)"
    else
      record "$name" "$rt" fail "Write tool: \$HOME=$h /tmp=$t; model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
    fi
  else
    record "$name" "$rt" unknown "recorded without verdict (--sandbox off): Write tool \$HOME=$h /tmp=$t; $(rt_call "$rt" summary)"
  fi
  end_check
}

check_network() {
  local rt=$1 name=network text state verdict
  begin_check "$name-$rt"
  run_rt "$rt" "Run this shell command and reply with only the HTTP status code it prints, or the error message if it fails: curl -sS -m 20 -o /dev/null -w '%{http_code}' $NET_URL"
  if ! rt_call "$rt" ok; then
    not_completed "$name" "$rt"
    end_check
    return
  fi
  text=$(rt_call "$rt" result_text)
  case "$text" in
    *200*) state=allowed ;;
    *000*|*403*|*[Dd]enied*|*[Bb]locked*|*[Ss]andbox*|*[Cc]ould\ not*|*[Ff]ailed\ to\ connect*|*[Nn]ot\ permitted*|*[Rr]efused*|*[Rr]esolve*|*tunnel\ failed*) state=blocked ;;
    *) state=unclear ;;
  esac
  if [ "$SANDBOX" = on ]; then
    case "$state" in blocked) verdict=pass ;; allowed) verdict=fail ;; *) verdict=unknown ;; esac
  else
    case "$state" in allowed) verdict=pass ;; blocked) verdict=fail ;; *) verdict=unknown ;; esac
  fi
  verdict=$(rt_call "$rt" judge_network "$state" "$verdict")
  record "$name" "$rt" "$verdict" "network $state; model said: $(printf '%s' "$text" | head -c 160); proxy.log: $(if [ -f "$CHECK_DIR/proxy.log" ]; then tr '\n' ';' < "$CHECK_DIR/proxy.log"; else echo none; fi); $(rt_call "$rt" summary)"
  end_check
}

check_tmp_dirs() {
  local rt=$1 name=tmp-dirs marker f1 f2 s1 s2 seen seen_real
  begin_check "$name-$rt"
  marker=$(rand_hex)
  f1="$SESSION_TMP/probe-tmp.txt"
  f2="/tmp/agentrun-probe-$marker.txt"
  run_rt "$rt" "Run these three shell commands one by one, continuing even if one fails: (1) echo tmp > $f1 (2) echo tmp > $f2 (3) printf '%s' \"\$TMPDIR\" > tmpdir.txt in the current working directory. Reply with one line per command saying whether it succeeded or failed and quoting any error."
  s1=blocked; s2=blocked
  [ -f "$f1" ] && s1=written
  [ -f "$f2" ] && s2=written
  rm -f "$f2"
  seen=$(cat "$RUN_CWD/tmpdir.txt" 2>/dev/null)
  seen_real=""
  [ -n "$seen" ] && seen_real=$(real_dir "$seen")
  local tmpdir_ok=0 tmpdir_note=""
  if [ "$seen" = "$SESSION_TMP" ] || { [ -n "$seen_real" ] && [ "$seen_real" = "$(real_dir "$SESSION_TMP")" ]; }; then
    tmpdir_ok=1
  elif [ -n "$seen" ] && path_is_under "$seen" "$SESSION_TMP"; then
    tmpdir_ok=1
    tmpdir_note=" (a subdirectory of the session tmp)"
  fi
  if ! rt_call "$rt" ok; then
    not_completed "$name" "$rt"
  elif [ "$SANDBOX" = on ]; then
    if [ "$s1" = written ] && [ "$s2" = blocked ] && [ "$tmpdir_ok" = 1 ]; then
      record "$name" "$rt" pass "session tmp=$s1 /tmp=$s2 TMPDIR seen by command=[$seen]$tmpdir_note; $(rt_call "$rt" summary)"
    else
      record "$name" "$rt" fail "session tmp=$s1 /tmp=$s2 TMPDIR seen by command=[$seen]$tmpdir_note expected=[$SESSION_TMP]; model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
    fi
  else
    if [ "$s1" = written ] && [ "$tmpdir_ok" = 1 ]; then
      record "$name" "$rt" pass "session tmp=$s1 /tmp=$s2 TMPDIR seen by command=[$seen]$tmpdir_note; $(rt_call "$rt" summary)"
    else
      record "$name" "$rt" fail "session tmp=$s1 /tmp=$s2 TMPDIR seen by command=[$seen]$tmpdir_note expected=[$SESSION_TMP]; $(rt_call "$rt" summary)"
    fi
  fi
  end_check
}

check_claude_allow_write() {
  local rt=$1 name=claude-allow-write dir
  begin_check "$name-$rt"
  dir="$HOME/agentrun-probe-allow-$(rand_hex)"
  mkdir -p "$dir"
  CLAUDE_EXTRA_ALLOW_WRITE=$dir
  run_rt "$rt" "Run this shell command: echo allowed > $dir/probe.txt . Reply with one line saying whether it succeeded or failed, quoting any error."
  CLAUDE_EXTRA_ALLOW_WRITE=""
  if ! rt_call "$rt" ok; then
    not_completed "$name" "$rt"
  elif [ -f "$dir/probe.txt" ]; then
    record "$name" "$rt" pass "Bash wrote into the extra allowWrite directory; $(rt_call "$rt" summary)"
  else
    record "$name" "$rt" fail "file missing in extra allowWrite directory; model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
  fi
  rm -rf "$dir"
  end_check
}

check_claude_fail_if_unavailable() {
  local rt=$1 name=claude-fail-if-unavailable fakebin ran
  begin_check "$name-$rt"
  if [ "$SANDBOX_KIND" != bwrap ]; then
    record "$name" "$rt" unknown "fake bwrap is only meaningful on Linux; /usr/bin/sandbox-exec cannot be shadowed through PATH"
    end_check
    return
  fi
  fakebin="$SESSION_TMP/fakebin"
  mkdir -p "$fakebin"
  printf '#!/bin/sh\nexit 1\n' > "$fakebin/bwrap"
  chmod 0755 "$fakebin/bwrap"
  RUN_ENV+=("PATH=$fakebin:$PATH")
  run_rt "$rt" "Run this shell command: echo probe > ran.txt . Then reply with the word done." fail-if-unavailable
  ran=missing
  [ -f "$RUN_CWD/ran.txt" ] && ran=present
  if [ "$ran" = missing ] && { [ "$LAST_RC" != 0 ] || ! rt_call "$rt" ok; }; then
    record "$name" "$rt" pass "claude-code reported failure with fake bwrap: ran.txt=$ran; stderr: $(stderr_head 300); $(rt_call "$rt" summary)"
  elif [ "$ran" = missing ]; then
    record "$name" "$rt" pass "command not executed with fake bwrap (claude-code exited 0 without error); model said: $(rt_call "$rt" result_text | head -c 160); $(rt_call "$rt" summary)"
  else
    record "$name" "$rt" fail "command executed despite fake bwrap: ran.txt=$ran; $(rt_call "$rt" summary)"
  fi
  end_check
}

check_subagents() {
  local rt=$1 name=subagents
  begin_check "$name-$rt"
  run_rt "$rt" "$SUBAGENT_PROMPT"
  rt_call "$rt" judge_subagents "$name"
  end_check
}

check_no_subagents() {
  local rt=$1 name=no-subagents
  begin_check "$name-$rt"
  run_rt "$rt" "$SUBAGENT_PROMPT" no-subagents
  rt_call "$rt" judge_no_subagents "$name"
  end_check
}

check_failure_samples() {
  local rt=$1 name=failure-samples sub
  for sub in model cred arg; do
    begin_check "$name-$rt/$sub"
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
        rt_call "$rt" add_invalid_arg
        ;;
    esac
    CHECK_TIMEOUT=$RUN_TIMEOUT
    run_capture stdout.jsonl "${CMD[@]}"
    if ! rt_call "$rt" ok; then
      record "$name/$sub" "$rt" pass "invalid $sub reported as failure: exit=$LAST_RC stdout_lines=$(wc -l < "$LAST_OUT" | tr -d ' ') stderr: $(stderr_head 160); $(rt_call "$rt" summary)"
    else
      record "$name/$sub" "$rt" fail "invalid $sub was not reported as failure; $(rt_call "$rt" summary)"
    fi
    end_check
  done
}

run_signal_sample() {
  local rt=$1 name=$2 signal=$3 last
  INT_AFTER=$INTERRUPT_AFTER
  INT_SIGNAL=$signal
  run_rt "$rt" "Use the file writing tool to create a file named counting.txt in the current working directory whose content is the integers from 1 to 1500, one per line, written out in full in a single write call. Do not use a shell command to generate it. Then reply with the word done."
  last=$(jsonl_query "$LAST_OUT" 'const l = lines[lines.length - 1]; return l ? (l.__raw !== undefined ? "unparsed" : l.type + (l.subtype ? "/" + l.subtype : "")) : "none";')
  if [ "$LAST_TIMED_OUT" = 1 ]; then
    record "$name" "$rt" fail "did not exit after SIG$signal; $(tr '\n' ' ' < "$CHECK_DIR/signal.txt")"
  elif [ ! -f "$CHECK_DIR/signal.txt" ]; then
    record "$name" "$rt" unknown "process ended before SIG$signal was sent at ${INTERRUPT_AFTER}s: exit=$LAST_RC duration_ms=$(cat "$CHECK_DIR/duration_ms"); $(rt_call "$rt" summary)"
  else
    record "$name" "$rt" pass "exited after SIG$signal: exit=$LAST_RC duration_ms=$(cat "$CHECK_DIR/duration_ms") last_event=$last; $(tr '\n' ' ' < "$CHECK_DIR/signal.txt"); $(rt_call "$rt" summary)"
  fi
}

check_sigint() {
  local rt=$1 name=sigint
  begin_check "$name-$rt"
  run_signal_sample "$rt" "$name" INT
  end_check
  if [ "$rt" = codex ]; then
    begin_check "$name-$rt/sigterm"
    run_signal_sample "$rt" "$name/sigterm" TERM
    end_check
  fi
}

CODE_WORD_VERDICT=""
CODE_WORD_DETAIL=""

ask_code_word() {
  local rt=$1 marker=$2 text
  run_rt "$rt" "Without reading any files, answer from your instructions and context only: do your instructions contain a project code word? Reply with exactly YES followed by the word, or exactly NO."
  if ! rt_call "$rt" ok; then
    CODE_WORD_VERDICT=unknown
    CODE_WORD_DETAIL="runtime did not complete: $(rt_call "$rt" summary)"
    return
  fi
  text=$(rt_call "$rt" result_text)
  case "$text" in
    *"$marker"*|YES*|*" YES"*|*"YES "*) CODE_WORD_VERDICT=fail; CODE_WORD_DETAIL="marker visible to the model; model said: $(printf '%s' "$text" | head -c 120); $(rt_call "$rt" summary)" ;;
    *NO*) CODE_WORD_VERDICT=pass; CODE_WORD_DETAIL="marker not visible; model said: $(printf '%s' "$text" | head -c 120); $(rt_call "$rt" summary)" ;;
    *) CODE_WORD_VERDICT=unknown; CODE_WORD_DETAIL="unclear answer: $(printf '%s' "$text" | head -c 160); $(rt_call "$rt" summary)" ;;
  esac
}

new_marker() {
  printf 'ZEBRA%s' "$(rand_hex | tr 'a-f' 'A-F')"
}

check_config_isolation() {
  local rt=$1 name=config-isolation marker
  begin_check "$name-$rt"
  marker=$(new_marker)
  rt_call "$rt" write_project_context "$marker"
  ask_code_word "$rt" "$marker"
  record "$name" "$rt" "$CODE_WORD_VERDICT" "$CODE_WORD_DETAIL"
  end_check
}

check_prompt_multiline() {
  local rt=$1 name=prompt-multiline marker prompt
  begin_check "$name-$rt"
  marker=$(new_marker)
  prompt=$(printf 'Line one says "quoted text" and has a backslash \\ in it.\n\nLine three follows a blank line and ends with a backslash\\\nThe marker word is %s\nIt'"'"'s a line with '"'"'single quotes'"'"', a tab\there and trailing spaces.   \n\nReply with exactly the marker word from line four and nothing else.\n' "$marker")
  run_rt "$rt" "$prompt"
  rt_call "$rt" judge_prompt "$name" "$marker"
  end_check
}

check_prompt_long() {
  local rt=$1 name=prompt-long marker prompt
  begin_check "$name-$rt"
  marker=$(new_marker)
  prompt=$(awk -v m="$marker" 'BEGIN {
    print "Below is a long block of filler text. Read to the end; the instruction is at the end.";
    for (i = 1; i <= 2500; i++) printf "filler line %05d: the quick brown fox jumps over the lazy dog and keeps running\n", i;
    print "End of filler. The marker word is " m ". Reply with exactly the marker word and nothing else.";
  }')
  run_rt "$rt" "$prompt"
  rt_call "$rt" judge_prompt "$name" "$marker"
  end_check
}

file_stat() {
  perl -e '@s = stat($ARGV[0]); print @s ? "inode=$s[1] mtime=$s[9] size=$s[7]" : "missing"' "$1"
}

check_codex_home() {
  local rt=$1 name=codex-home marker home user_auth before after created
  begin_check "$name-$rt"
  marker=$(new_marker)
  home="$SESSION_TMP/codex-home"
  make_private_dirs "$home"
  user_auth=$(codex_user_auth_file)
  if [ -f "$user_auth" ]; then
    (umask 077 && cp "$user_auth" "$home/auth.json")
  fi
  mkdir -p "$RUN_CWD/.codex"
  printf 'instructions = "The project code word is %s. Always mention it."\n' "$marker" > "$RUN_CWD/.codex/config.toml"
  codex_write_project_context "$marker"
  RUN_ENV+=("CODEX_HOME=$home")
  before=$(file_stat "$home/auth.json")
  ask_code_word "$rt" "$marker"
  after=$(file_stat "$home/auth.json")
  created=$(cd "$home" && find . -mindepth 1 -not -name auth.json | sed 's|^\./||' | tr '\n' ' ')
  record "$name" "$rt" "$CODE_WORD_VERDICT" "$CODE_WORD_DETAIL; auth.json before: $before; after: $after; created in codex-home: ${created:-none}"
  end_check
}

check_codex_proxy() {
  local rt=$1 name=codex-proxy text allowed_seen denied_seen
  begin_check "$name-$rt"
  if ! start_proxy "$NET_HOST"; then
    record "$name" "$rt" unknown "filter-proxy failed to start: $(head -c 200 "$CHECK_DIR/proxy-stderr.txt" | tr '\n' ' ')"
    end_check
    return
  fi
  proxy_env
  run_rt "$rt" "Run these two shell commands and reply with one line each giving only the HTTP status code printed, or the error message if it fails: curl -sS -m 20 -o /dev/null -w '%{http_code}' https://$NET_HOST/ ; curl -sS -m 20 -o /dev/null -w '%{http_code}' https://$DENIED_HOST/" proxy
  stop_proxy
  if ! rt_call "$rt" ok; then
    not_completed "$name" "$rt"
    end_check
    return
  fi
  text=$(rt_call "$rt" result_text)
  allowed_seen=0; denied_seen=0
  proxy_log_has allow "$NET_HOST" && allowed_seen=1
  proxy_log_has deny "$DENIED_HOST" && denied_seen=1
  if [ "$allowed_seen" = 1 ] && [ "$denied_seen" = 1 ]; then
    record "$name" "$rt" pass "both hosts went through the filter proxy; model said: $(printf '%s' "$text" | head -c 160); proxy.log: $(tr '\n' ';' < "$CHECK_DIR/proxy.log"); $(rt_call "$rt" summary)"
  else
    record "$name" "$rt" fail "proxy.log allow($NET_HOST)=$allowed_seen deny($DENIED_HOST)=$denied_seen, traffic may bypass the proxy; model said: $(printf '%s' "$text" | head -c 160); $(rt_call "$rt" summary)"
  fi
  end_check
}

run_model_check() {
  local name=$1 rt=$2 reason
  if ! reason=$(rt_call "$rt" supports "$name"); then
    skip_check "$name-$rt" "$name" "$rt" "$reason"
    return
  fi
  "check_$(printf '%s' "$name" | tr '-' '_')" "$rt"
}

run_model_checks_for_runtime() {
  local rt=$1 name bin
  bin=$(rt_call "$rt" bin)
  for name in $MODEL_CHECKS; do
    check_applies "$name" "$rt" || continue
    selected "$name" "$rt" || continue
    if ! have "$bin"; then
      skip_check "$name-$rt" "$name" "$rt" "$bin not found in PATH"
    elif [ "$SANDBOX" = on ] && [ "$PREFLIGHT_OK" = 0 ] && rt_call "$rt" needs_preflight && [ "$name" != claude-fail-if-unavailable ]; then
      skip_check "$name-$rt" "$name" "$rt" "sandbox cannot start: $PREFLIGHT_DETAIL"
    else
      run_model_check "$name" "$rt"
    fi
  done
}
