# shellcheck shell=bash disable=SC2034,SC2153

ALL_SCENARIOS="sandbox-check dry-run usage-error relax-note basic-task outside-write session-tmp network-none network-custom network-full webfetch subagents no-subagents config-isolation fail-model fail-arg fail-credential interrupt timeout cleanup doctor"
SANDBOX_SCENARIOS="outside-write session-tmp network-none network-custom network-full"
GATE_EXEMPT_SCENARIOS="dry-run usage-error sandbox-check relax-note doctor"

BASIC_PROMPT="Create a file named accept.txt containing the single word hello, then read it back."
SHELL_ONCE="Run exactly this shell command once with your shell tool, as one command, and do not try any other way to do the same thing:"
NO_RETRY="If the command fails, do not retry it and do not try another way. Then reply with one line."
LONG_COMMAND="node -e 'setTimeout(function () {}, 120000)'"
SLEEPER_COMMAND="node -e \"require('child_process').spawn('node', ['-e', 'setTimeout(function () {}, 311000)'], { stdio: 'ignore' }).unref()\"; echo started > started.txt"

SANDBOX_AVAILABLE=""
SANDBOX_UNAVAILABLE_DETAIL=""
SANDBOX_RUNS=0
SANDBOX_GATE=""
EXPECTED_SANDBOX=none

scenario_applies() {
  local name=$1 rt=$2
  case "$name" in
    relax-note) [ "$rt" != codex ] ;;
    webfetch) [ "$rt" = claude-code ] ;;
    subagents|no-subagents) [ "$rt" != pi ] ;;
    *) return 0 ;;
  esac
}

in_list() {
  case " $2 " in
    *" $1 "*) return 0 ;;
  esac
  return 1
}

sandbox_kind_from_command() {
  local rt=$1 command=$2
  case "$rt" in
    pi)
      case "$command" in
        *sandbox-exec*) printf 'seatbelt' ;;
        *bwrap*) printf 'bubblewrap' ;;
        *) printf 'none' ;;
      esac
      ;;
    claude-code)
      case "$command" in
        *'"sandbox":{"enabled":true'*) printf '%s' "$OS_SANDBOX" ;;
        *) printf 'none' ;;
      esac
      ;;
    codex)
      case "$command" in
        *default_permissions=agentrun*) printf 'codex' ;;
        *) printf 'none' ;;
      esac
      ;;
  esac
}

sandbox_excerpt() {
  local rt=$1 command=$2
  case "$rt" in
    pi) printf '%s' "${command%% /bin/sh -c*}" | sed 's/^command: //' ;;
    claude-code) printf '%s' "$command" | sed "s/.*--settings '\([^']*\)'.*/\1/" ;;
    codex) printf '%s' "$command" | sed 's/.*\(-c default_permissions=agentrun\).*/\1/' ;;
  esac
}

dry_run_into() {
  local rec=$1
  mkdir -p "$rec"
  WANT_FORMAT=jsonl
  WANT_TIMEOUT=""
  run_agentrun "$rec" "$WORK" hi --dry-run
}

probe_sandbox() {
  local rt=$1 rec=$2 line
  SANDBOX_AVAILABLE=""
  SANDBOX_UNAVAILABLE_DETAIL=""
  SANDBOX_RUNS=0
  SANDBOX_GATE=""
  EXPECTED_SANDBOX=none
  WANT_SANDBOX=on
  dry_run_into "$rec"
  if [ "$LAST_RC" = 0 ]; then
    SANDBOX_AVAILABLE=1
  elif [ "$LAST_RC" = 2 ]; then
    line=$(end_detail)
    case "$line" in
      "sandbox is not available: "*)
        SANDBOX_AVAILABLE=0
        SANDBOX_UNAVAILABLE_DETAIL=$line
        ;;
      *) SANDBOX_UNAVAILABLE_DETAIL="rejected for another reason: $line" ;;
    esac
  else
    SANDBOX_UNAVAILABLE_DETAIL="--dry-run exited with $LAST_RC: $(stderr_tail)"
  fi
  if [ "$SANDBOX" = on ]; then
    [ "$SANDBOX_AVAILABLE" = 1 ] && SANDBOX_RUNS=1
    [ "$SANDBOX_AVAILABLE" = 0 ] && SANDBOX_GATE="sandbox not available (--sandbox on)"
  elif [ "$SANDBOX" = relax ]; then
    [ "$SANDBOX_AVAILABLE" = 1 ] && SANDBOX_RUNS=1
  fi
  if [ "$SANDBOX_RUNS" = 1 ]; then
    EXPECTED_SANDBOX=$OS_SANDBOX
    [ "$rt" = codex ] && EXPECTED_SANDBOX=codex
  fi
}

scenario_dry_run() {
  local first note=""
  WANT_TIMEOUT=""
  if [ -n "$SANDBOX_GATE" ]; then
    WANT_SANDBOX=relax
    note=" (--sandbox relax because the sandbox is not available)"
  fi
  run_in_work hi --dry-run
  first=$(first_line "$LAST_OUT")
  if [ "$LAST_RC" = 0 ] && [ "${first#command: }" != "$first" ]; then
    pass "exit 0, no model call, command: $(printf '%s' "${first#command: }" | head -c 60)...$note"
  else
    fail "exit $LAST_RC, first line: $(printf '%s' "$first" | head -c 200); stderr: $(stderr_tail)"
  fi
}

scenario_usage_error() {
  local expected="agentrun: --network custom requires at least one --allow-host"
  run_in_work hi --network custom
  if [ "$LAST_RC" = 2 ] && [ "$(ev_line_count)" = 1 ] && [ "$(ev_first end status)" = rejected ] && stderr_has_line "$expected"; then
    pass "exit 2, one rejected end event, stderr line present"
  else
    fail "exit $LAST_RC, events: $(ev_types), end.status $(end_status), stderr: $(stderr_tail)"
  fi
}

scenario_sandbox_check() {
  local command
  if [ "$SANDBOX" = on ]; then
    probe_sandbox "$RT" "$REC"
    if [ "$SANDBOX_AVAILABLE" = 1 ]; then
      command=$(first_line "$LAST_OUT")
      pass "$EXPECTED_SANDBOX: $(sandbox_excerpt "$RT" "$command")"
    elif [ "$SANDBOX_AVAILABLE" = 0 ]; then
      pass "$SANDBOX_UNAVAILABLE_DETAIL"
    else
      fail "$SANDBOX_UNAVAILABLE_DETAIL"
    fi
    return
  fi
  probe_sandbox "$RT" "$REC/on"
  reset_wants
  dry_run_into "$REC"
  if [ "$LAST_RC" != 0 ]; then
    fail "--sandbox $SANDBOX --dry-run exited with $LAST_RC: $(end_detail) $(stderr_tail)"
    return
  fi
  command=$(first_line "$LAST_OUT")
  if [ "$(sandbox_kind_from_command "$RT" "$command")" != "$EXPECTED_SANDBOX" ]; then
    fail "--sandbox $SANDBOX --dry-run command shows $(sandbox_kind_from_command "$RT" "$command"), expected $EXPECTED_SANDBOX: $command"
    return
  fi
  if [ "$SANDBOX" = off ]; then
    skip "--sandbox off (sandbox available with --sandbox on: $(availability_word))"
  elif [ "$SANDBOX_AVAILABLE" = 1 ]; then
    pass "$EXPECTED_SANDBOX: $(sandbox_excerpt "$RT" "$command")"
  elif [ "$SANDBOX_AVAILABLE" = 0 ]; then
    pass "relax without a sandbox: $SANDBOX_UNAVAILABLE_DETAIL"
  else
    fail "$SANDBOX_UNAVAILABLE_DETAIL"
  fi
}

availability_word() {
  case "$SANDBOX_AVAILABLE" in
    1) printf 'yes' ;;
    0) printf 'no' ;;
    *) printf 'unknown' ;;
  esac
}

scenario_relax_note() {
  local first last
  if [ "$SANDBOX_AVAILABLE" = 1 ]; then
    skip "sandbox available"
    return
  elif [ "$SANDBOX_AVAILABLE" != 0 ]; then
    skip "sandbox availability unknown: $SANDBOX_UNAVAILABLE_DETAIL"
    return
  fi
  WANT_SANDBOX=relax
  WANT_FORMAT=text
  run_in_work "$BASIC_PROMPT"
  first=$(first_line "$LAST_OUT")
  last=$(last_line "$LAST_OUT")
  case "$first" in
    "[note] sandbox not running (--sandbox relax: "*)
      case "$last" in
        "[end] finished"*) pass "note line and finished: $first" ;;
        *) fail "note line present but last line is: $last" ;;
      esac
      ;;
    *) fail "first line is: $first; exit $LAST_RC; stderr: $(stderr_tail)" ;;
  esac
}

scenario_basic_task() {
  local types input
  run_in_work "$BASIC_PROMPT"
  if ! finished_ok; then
    fail "$(failure_summary)"
    return
  fi
  if [ "$(file_word "$WORK/accept.txt")" != hello ]; then
    fail "accept.txt content is '$(file_word "$WORK/accept.txt")'"
    return
  fi
  types=$(ev_types)
  case "$types" in
    "start prompt "*" end") ;;
    *) fail "event order: $types"; return ;;
  esac
  if [ "$(ev_first prompt text)" != "$BASIC_PROMPT" ]; then
    fail "prompt.text differs: $(ev_first prompt text)"
    return
  fi
  if [ "$(ev_count tool)" = 0 ] || [ "$(ev_count text)" = 0 ] || [ "$(ev_count usage)" = 0 ]; then
    fail "missing events: $types"
    return
  fi
  input=$(ev_last end usage.input_tokens)
  if ! [ "${input:-0}" -gt 0 ] 2>/dev/null; then
    fail "end.usage.input_tokens is '$input'"
    return
  fi
  if [ "$(ev_first start sandbox)" != "$EXPECTED_SANDBOX" ]; then
    fail "start.sandbox is $(ev_first start sandbox), env.txt expects $EXPECTED_SANDBOX"
    return
  fi
  pass "accept.txt written, events $types, input_tokens $input, sandbox $(ev_first start sandbox)"
}

scenario_outside_write() {
  local suffix home_file tmp_file written=""
  suffix=$(rand_hex)
  home_file="$HOME/agentrun-accept-$suffix"
  tmp_file="/tmp/agentrun-accept-$suffix"
  run_in_work "$SHELL_ONCE echo x > \"\$HOME/agentrun-accept-$suffix\"; echo x > /tmp/agentrun-accept-$suffix; echo done > done.txt $NO_RETRY"
  [ -e "$home_file" ] && written="$home_file"
  [ -e "$tmp_file" ] && written="$written $tmp_file"
  rm -f "$home_file" "$tmp_file"
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif [ ! -f "$WORK/done.txt" ]; then
    fail "done.txt missing; outside files written:${written:- none}"
  elif [ -n "$written" ]; then
    fail "written outside the working directory:$written (deleted)"
  else
    pass "done.txt written, \$HOME and /tmp files not created"
  fi
}

scenario_session_tmp() {
  local tempdir written note="" variable
  WANT_DEBUG=1
  run_in_work "$SHELL_ONCE echo ok > \"\$TMPDIR/accept-tmp.txt\"; printf %s \"\$TMPDIR\" > tmpdir.txt $NO_RETRY"
  tempdir=$(stderr_line_starting '\[debug\] tempdir: ')
  written=$(cat "$WORK/tmpdir.txt" 2>/dev/null)
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif [ -z "$tempdir" ]; then
    fail "no [debug] tempdir: line in stderr"
  elif [ -z "$written" ]; then
    fail "tmpdir.txt missing or empty (session tempdir $tempdir)"
  elif [ ! -f "$written/accept-tmp.txt" ]; then
    fail "accept-tmp.txt not found in $written (session tempdir $tempdir)"
  else
    case "$written" in
      "$tempdir"|"$tempdir"/*) note="TMPDIR $written is inside the session tempdir" ;;
      *)
        if [ "$RT" = claude-code ] && [ "$OS_NAME" = macos ]; then
          note="claude-code TMPDIR is $written (accepted on macOS)"
        else
          fail "TMPDIR $written is outside the session tempdir $tempdir"
        fi
        ;;
    esac
    if [ -n "$note" ]; then
      variable=""
      [ "$RT" = pi ] && [ -n "${AGENTRUN_PI_AUTH:-}" ] && variable=AGENTRUN_PI_AUTH
      [ "$RT" = codex ] && [ -n "${AGENTRUN_CODEX_AUTH:-}" ] && variable=AGENTRUN_CODEX_AUTH
      if [ -n "$variable" ] && ! grep -q -- "^\[debug\] credentials: $variable -> " "$LAST_ERR"; then
        fail "$note; no [debug] credentials: $variable -> line"
      else
        [ -n "$variable" ] && note="$note; credentials line for $variable present"
        pass "$note"
      fi
    fi
  fi
  remove_kept_dirs
}

remove_kept_dirs() {
  local dir
  for dir in "$(stderr_line_starting '\[debug\] tempdir: ')" "$(stderr_line_starting '\[debug\] codex home: ')"; do
    case "$dir" in
      "$BASE_TMP"/agentrun-*) rm -rf "$dir" ;;
    esac
  done
}

curl_to() {
  printf "curl -sS -m %s -o /dev/null -w '%%{http_code}' %s > %s" "$1" "$2" "$3"
}

pi_service_host() {
  case "${PI_MODEL%%/*}" in
    deepseek) printf 'api.deepseek.com' ;;
    anthropic) printf 'api.anthropic.com' ;;
    openai) printf 'api.openai.com' ;;
    google) printf 'generativelanguage.googleapis.com' ;;
    openrouter) printf 'openrouter.ai' ;;
  esac
}

scenario_network_none() {
  local code denied service host
  run_in_work "$SHELL_ONCE $(curl_to 10 https://api.github.com/ code.txt) $NO_RETRY"
  code=$(file_word "$WORK/code.txt")
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif [ ! -f "$WORK/code.txt" ]; then
    fail "code.txt missing"
  elif [ "$code" = 200 ]; then
    fail "api.github.com answered 200 with --network none"
  elif [ "$RT" = pi ]; then
    denied=$(ev_network api.github.com 443)
    host=$(pi_service_host)
    if [ "$denied" != "allowed=false reason=not_allowed" ]; then
      fail "code.txt is '$code'; network event for api.github.com:443 is '${denied:-missing}'"
    elif [ -z "$host" ]; then
      pass "code.txt is '$code', api.github.com:443 denied; model service host of ${PI_MODEL%%/*} unknown, not checked"
    else
      service=$(ev_network "$host" 443)
      case "$service" in
        "allowed=true"*) pass "code.txt is '$code', api.github.com:443 denied, $host:443 allowed" ;;
        *) fail "code.txt is '$code', api.github.com:443 denied, but $host:443 event is '${service:-missing}'" ;;
      esac
    fi
  else
    pass "code.txt is '$code'"
  fi
}

scenario_network_custom() {
  local a b last
  WANT_FORMAT=text
  [ "$RT" = codex ] && WANT_DEBUG=1
  run_in_work "$SHELL_ONCE $(curl_to 10 https://api.github.com/ a.txt); $(curl_to 10 https://example.com/ b.txt) $NO_RETRY" --network custom --allow-host api.github.com
  remove_kept_dirs
  a=$(file_word "$WORK/a.txt")
  b=$(file_word "$WORK/b.txt")
  last=$(last_line "$LAST_OUT")
  case "$last" in
    "[end] finished"*) ;;
    *) fail "last line: $last; exit $LAST_RC; stderr: $(stderr_tail)"; return ;;
  esac
  if [ "$a" != 200 ]; then
    fail "a.txt (api.github.com) is '${a:-missing}'"
  elif [ "$b" = 200 ] || [ -z "$b" ]; then
    fail "b.txt (example.com) is '${b:-missing}'"
  elif [ "$RT" = codex ]; then
    if grep -q -- '--enable network_proxy' "$LAST_ERR"; then
      pass "a.txt 200, b.txt $b, --enable network_proxy in the command"
    else
      fail "a.txt 200, b.txt $b, but --enable network_proxy not in the [debug] command line"
    fi
  elif grep -q -x -F -- '[main] net denied example.com:443 (not_allowed)' "$LAST_OUT"; then
    pass "a.txt 200, b.txt $b, net denied line present"
  else
    fail "a.txt 200, b.txt $b, but no '[main] net denied example.com:443 (not_allowed)' line"
  fi
}

scenario_network_full() {
  local a b event
  run_in_work "$SHELL_ONCE $(curl_to 10 https://api.github.com/ a.txt); $(curl_to 5 http://169.254.169.254/ b.txt) $NO_RETRY" --network full
  a=$(file_word "$WORK/a.txt")
  b=$(file_word "$WORK/b.txt")
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif [ "$a" != 200 ]; then
    fail "a.txt (api.github.com) is '${a:-missing}'"
  elif [ "$RT" = codex ]; then
    pass "a.txt 200; 169.254.169.254 answered '${b:-nothing}' (not judged, codex full bypasses the filter proxy)"
  elif [ "$b" = 200 ] || [ -z "$b" ]; then
    fail "b.txt (169.254.169.254) is '${b:-missing}'"
  else
    event=$(ev_network 169.254.169.254 80)
    if [ "$event" = "allowed=false reason=private_address" ]; then
      pass "a.txt 200, b.txt $b, 169.254.169.254:80 denied as private_address"
    elif [ "$RT" = claude-code ] && [ -z "$event" ]; then
      pass "a.txt 200, b.txt $b; no network event: claude-code's sandbox does not send requests to IP literals through the proxy, the request failed inside the network namespace"
    else
      fail "a.txt 200, b.txt $b, network event for 169.254.169.254:80 is '${event:-missing}'"
    fi
  fi
}

scenario_webfetch() {
  local allowed denied
  run_in_work "Use the WebFetch tool to read https://example.com/ and then use the WebFetch tool to read https://www.iana.org/. Attempt both even if the first fails or is denied. Then reply with one line per site saying whether it worked." --network custom --allow-host example.com
  allowed=$(ev_tool_denied WebFetch example.com)
  denied=$(ev_tool_denied WebFetch www.iana.org)
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif [ "$allowed" = false ] && [ "$denied" = true ]; then
    pass "WebFetch example.com allowed, www.iana.org denied"
  else
    fail "WebFetch example.com denied=${allowed:-missing}, www.iana.org denied=${denied:-missing}"
  fi
}

subagent_prompt() {
  if [ "$RT" = codex ]; then
    printf 'Start two subagents in parallel: the first creates a file named sub1.txt in the current working directory containing the word one, the second creates sub2.txt containing the word two. Wait for both to finish, then reply done. Do not create the files yourself; if you cannot start subagents, say so.'
  else
    printf 'Start one subagent that creates a file named sub.txt in the current working directory containing the word sub. Wait for it to finish, then reply done. Do not create the file yourself; if you cannot start subagents, say so.'
  fi
}

scenario_subagents() {
  local status
  run_in_work "$(subagent_prompt)"
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif [ "$RT" = codex ]; then
    if [ ! -f "$WORK/sub1.txt" ] || [ ! -f "$WORK/sub2.txt" ]; then
      fail "sub1.txt or sub2.txt missing"
    elif [ "$(ev_count subagent_start)" != 0 ]; then
      fail "codex emitted subagent_start"
    else
      pass "sub1.txt and sub2.txt written, no subagent_start (codex has no subagent events)"
    fi
  elif [ ! -f "$WORK/sub.txt" ]; then
    fail "sub.txt missing; subagent_start $(ev_count subagent_start)"
  else
    status=$(ev_subagent_end_status)
    if [ "$(ev_count subagent_start)" = 0 ]; then
      fail "sub.txt written but no subagent_start"
    elif [ "$status" != finished ]; then
      fail "subagent_end status: $status"
    else
      pass "sub.txt written, subagent_start and subagent_end finished"
    fi
  fi
}

scenario_no_subagents() {
  local tools argv
  run_in_work "$(subagent_prompt)" --no-subagents
  if [ "$(ev_count subagent_start)" != 0 ]; then
    fail "subagent_start present with --no-subagents"
  elif [ "$RT" = codex ]; then
    argv=$(ev_argv_joined)
    case "$argv" in
      *agents.enabled=false*) pass "no subagent_start, argv has agents.enabled=false ($(run_summary))" ;;
      *) fail "argv lacks agents.enabled=false: $argv" ;;
    esac
  else
    tools=$(ev_argv_value --tools)
    if [ -z "$tools" ]; then
      fail "no --tools in start.argv ($(failure_summary))"
    else
      case ",$tools," in
        *,Task,*) fail "--tools still contains Task: $tools" ;;
        *) pass "no subagent_start, --tools is $tools ($(run_summary))" ;;
      esac
    fi
  fi
}

scenario_config_isolation() {
  local word home prompt read_by_tool
  word="w$(rand_hex)"
  prompt="What is the project code word? Reply UNKNOWN if you do not know it. Answer only from what you already know: do not list directories, read files or run commands."
  printf 'The project code word is %s. Always mention it.\n' "$word" > "$WORK/CLAUDE.md"
  cp "$WORK/CLAUDE.md" "$WORK/AGENTS.md"
  if [ "$RT" = codex ]; then
    home="$PRIVATE_ROOT/codex-home-$(rand_hex)"
    mkdir -m 0700 "$home"
    cp "$WORK/AGENTS.md" "$home/AGENTS.md"
    ln -s "${CODEX_HOME:-$HOME/.codex}/auth.json" "$home/auth.json"
    run_in_work "$prompt" --env "CODEX_HOME=$home"
  else
    run_in_work "$prompt"
  fi
  read_by_tool=$(jsonl_query "$LAST_OUT" 'return events.some((e) => e.type === "tool" && /CLAUDE\.md|AGENTS\.md/.test(e.summary)) ? "yes" : "";')
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif ! grep -q -- "$word" "$LAST_OUT"; then
    pass "code word not in the output; reply: $(ev_last end result | head -c 80)"
  elif [ -n "$read_by_tool" ]; then
    skip "the agent read CLAUDE.md or AGENTS.md with a tool despite the prompt, so the output cannot show whether the files were loaded; run again"
  else
    fail "the code word from CLAUDE.md/AGENTS.md appeared in the output without a tool reading them"
  fi
}

failed_run() {
  [ "$LAST_RC" = 1 ] && [ "$(end_status)" = failed ]
}

scenario_fail_model() {
  case "$RT" in
    pi) WANT_MODEL=deepseek/no-such-model-accept ;;
    *) WANT_MODEL=no-such-model-accept ;;
  esac
  run_in_work hi
  if failed_run && [ -n "$(end_detail)" ]; then
    pass "exit 1, failed: $(end_detail | head -c 100)"
  else
    fail "$(failure_summary)"
  fi
}

scenario_fail_arg() {
  run_in_work hi -- --no-such-flag-accept
  if failed_run; then
    pass "exit 1, failed: $(end_detail | head -c 100)"
  else
    fail "$(failure_summary)"
  fi
}

scenario_fail_credential() {
  local dir
  case "$RT" in
    claude-code)
      run_in_work hi --env CLAUDE_CODE_OAUTH_TOKEN=invalid-accept
      ;;
    pi)
      dir="$PRIVATE_ROOT/pi-state-$(rand_hex)"
      mkdir -m 0700 "$dir"
      printf '{"defaultProvider":"deepseek","defaultModel":"deepseek-flash"}\n' > "$dir/settings.json"
      run_in_work hi --env "PI_CODING_AGENT_DIR=$dir" --env DEEPSEEK_API_KEY=invalid-accept
      ;;
    codex)
      dir="$PRIVATE_ROOT/codex-home-$(rand_hex)"
      mkdir -m 0700 "$dir"
      printf '{"OPENAI_API_KEY":"invalid"}\n' > "$dir/auth.json"
      run_in_work hi --env "CODEX_HOME=$dir"
      ;;
  esac
  if failed_run; then
    pass "exit 1, failed: $(end_detail | head -c 100)"
  else
    fail "$(failure_summary)"
  fi
}

interrupt_once() {
  local signal=$1 code=$2 rec
  rec="$REC/$signal"
  mkdir -p "$rec"
  WANT_SIGNAL=$signal
  run_agentrun "$rec" "$WORK" "$SHELL_ONCE $LONG_COMMAND $NO_RETRY"
  if [ "$LAST_RC" = "$code" ] && [ "$(end_status)" = interrupted ] && [ "$LAST_SIGNAL_MS" -le 8000 ]; then
    printf 'SIG%s: exit %s, interrupted, exited %sms after the signal' "$signal" "$LAST_RC" "$LAST_SIGNAL_MS"
    return 0
  fi
  printf 'SIG%s: exit %s (expected %s), end.status %s, exited %sms after the signal' "$signal" "$LAST_RC" "$code" "$(end_status)" "$LAST_SIGNAL_MS"
  return 1
}

scenario_interrupt() {
  local int term ok=1
  int=$(interrupt_once INT 130) || ok=0
  term=$(interrupt_once TERM 143) || ok=0
  if [ "$ok" = 1 ]; then
    pass "$int; $term"
  else
    fail "$int; $term"
  fi
}

scenario_timeout() {
  WANT_TIMEOUT=10
  run_in_work "$SHELL_ONCE $LONG_COMMAND $NO_RETRY"
  if [ "$LAST_RC" = 3 ] && [ "$(end_status)" = timeout ] && [ "$LAST_MS" -le 20000 ]; then
    pass "exit 3, timeout, exited after ${LAST_MS}ms"
  else
    fail "exit $LAST_RC, end.status $(end_status), exited after ${LAST_MS}ms"
  fi
}

tmp_dirs() {
  find "$BASE_TMP" -mindepth 1 -maxdepth 1 -name 'agentrun-*' 2>/dev/null | sort | tr '\n' ' '
}

sleepers() {
  ps -A -o pid= -o args= | awk '/set[T]imeout/ && /31[1]000/ { print $1 }'
}

scenario_cleanup() {
  local before after pids
  before=$(tmp_dirs)
  run_in_work "$SHELL_ONCE $SLEEPER_COMMAND $NO_RETRY"
  sleep 1
  after=$(tmp_dirs)
  pids=$(sleepers)
  if [ -n "$pids" ]; then
    # shellcheck disable=SC2086
    kill -KILL $pids 2>/dev/null
  fi
  if [ "$(end_status)" != finished ]; then
    fail "$(failure_summary)"
  elif [ ! -f "$WORK/started.txt" ]; then
    fail "started.txt missing"
  elif [ -n "$pids" ]; then
    fail "the background node process (setTimeout 311000) was still running after agentrun exited (pid $(printf '%s' "$pids" | tr '\n' ' ')), killed"
  elif [ "$before" != "$after" ]; then
    fail "agentrun-* directories under $BASE_TMP changed: before [$before] after [$after]"
  else
    pass "started.txt written, the background node process is gone, no agentrun-* directory left under $BASE_TMP"
  fi
}

DOCTOR_QUERY='
const fs = require("fs");
let items = [];
try { items = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); } catch (e) { process.exit(0); }
if (!Array.isArray(items)) process.exit(0);
const mode = process.argv[2];
const status = (check) => { const i = items.find((x) => x.check === check); return i ? i.status : "missing"; };
const detail = (check) => { const i = items.find((x) => x.check === check); return i ? i.detail : ""; };
const fails = items.filter((x) => x.status === "fail").map((x) => x.check + ": " + x.detail);
if (mode === "fails") process.stdout.write(fails.join("; "));
else if (mode === "status") process.stdout.write(status(process.argv[3]));
else if (mode === "detail") process.stdout.write(detail(process.argv[3]));
else if (mode === "count") process.stdout.write(String(items.length));
'

doctor_query() {
  node -e "$DOCTOR_QUERY" "$@" 2>/dev/null
}

run_doctor() {
  local out=$1
  shift
  local cmd=("$AGENTRUN" doctor "$RT" --json --sandbox "$SANDBOX" "$@")
  {
    printf 'cmd:'
    printf ' %q' "${cmd[@]}"
    printf '\n'
  } >> "$REC/cmd.txt"
  (cd "$WORK" && exec "${cmd[@]}") > "$REC/$out.json" 2> "$REC/$out.stderr.txt" < /dev/null
  LAST_RC=$?
  printf '%s\n' "$LAST_RC" >> "$REC/exit_code"
}

scenario_doctor() {
  local first second fails problem="" sandbox network
  : > "$REC/cmd.txt"
  : > "$REC/exit_code"
  first="$REC/doctor-1.json"
  second="$REC/doctor-2.json"
  run_doctor doctor-1
  if [ "$(doctor_query "$first" count)" = "" ]; then
    fail "doctor --json did not produce a JSON array (exit $LAST_RC): $(head -c 200 "$REC/doctor-1.stderr.txt" | tr '\n' ' ')"
    return
  fi
  fails=$(doctor_query "$first" fails)
  sandbox=$(doctor_query "$first" status sandbox)
  network=$(doctor_query "$first" status network)
  if [ "$SANDBOX" = off ]; then
    if [ "$sandbox" != skip ] || [ "$network" != skip ]; then
      problem="with --sandbox off expected sandbox and network skip, got sandbox $sandbox, network $network"
    elif [ -n "$fails" ]; then
      problem="fail items: $fails"
    fi
  elif [ "$SANDBOX" = on ] && [ "$SANDBOX_AVAILABLE" = 0 ]; then
    if [ "$sandbox" != fail ]; then
      problem="sandbox is $sandbox, expected fail because the sandbox is not available"
    else
      case "$(doctor_query "$first" detail sandbox)" in
        *"--sandbox off"*) ;;
        *) problem="sandbox fail detail lacks --sandbox off: $(doctor_query "$first" detail sandbox)" ;;
      esac
      [ -z "$problem" ] && [ "$fails" != "sandbox: $(doctor_query "$first" detail sandbox)" ] && problem="other fail items: $fails"
    fi
  elif [ -n "$fails" ]; then
    problem="fail items: $fails"
  fi
  if [ -n "$problem" ]; then
    fail "first run (exit $LAST_RC): $problem"
    return
  fi
  run_doctor doctor-2 --network custom --allow-host api.github.com
  network=$(doctor_query "$second" status network)
  if [ "$(doctor_query "$second" count)" = "" ]; then
    fail "second run: doctor --json did not produce a JSON array (exit $LAST_RC): $(head -c 200 "$REC/doctor-2.stderr.txt" | tr '\n' ' ')"
  elif [ "$SANDBOX_RUNS" = 1 ] && [ "$network" != ok ]; then
    fail "second run: network is $network, expected ok: $(doctor_query "$second" detail network)"
  elif [ "$SANDBOX_RUNS" = 0 ] && [ "$network" != skip ]; then
    fail "second run: network is $network, expected skip: $(doctor_query "$second" detail network)"
  else
    pass "first run: sandbox $sandbox, no unexpected fail${fails:+ ($fails)}; second run: network $network ($(doctor_query "$second" detail network))"
  fi
}

run_scenario() {
  local name=$1 rt=$2
  begin_scenario "$name" "$rt"
  if [ -n "$SANDBOX_GATE" ] && ! in_list "$name" "$GATE_EXEMPT_SCENARIOS"; then
    skip "$SANDBOX_GATE"
    return
  fi
  if in_list "$name" "$SANDBOX_SCENARIOS" && [ "$SANDBOX_RUNS" != 1 ]; then
    skip "sandbox not running"
    return
  fi
  "scenario_${name//-/_}" "$rt"
}

run_runtime() {
  local rt=$1 name
  for name in $ALL_SCENARIOS; do
    scenario_applies "$name" "$rt" || continue
    if [ "$name" != sandbox-check ] && ! selected "$name"; then
      continue
    fi
    run_scenario "$name" "$rt"
  done
}
