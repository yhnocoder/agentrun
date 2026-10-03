# shellcheck shell=bash disable=SC2034

codex_bin() {
  echo codex
}

codex_build_cmd() {
  local prompt=$1 mode=$2
  CMD=(codex exec --json --skip-git-repo-check)
  if [ "$PLATFORM" = docker ] && [ "$mode" != own-sandbox ]; then
    CMD+=(--sandbox danger-full-access)
  else
    CMD+=(--sandbox workspace-write)
  fi
  CMD+=("$prompt")
}

codex_ok() {
  [ "$LAST_RC" = 0 ] && ! grep -q '"turn.failed"' "$LAST_OUT" 2>/dev/null
}

codex_summary() {
  echo "exit=$LAST_RC events: $(event_types) usage=$(grep '"turn.completed"' "$LAST_OUT" 2>/dev/null | tail -1 | grep -o '"usage":{[^}]*' | grep -o '"[a-z_]*":' | tr -d '":' | tr '\n' ',') turn_failed=$(grep -c '"turn.failed"' "$LAST_OUT" 2>/dev/null)"
}

codex_result_text() {
  grep '"item.completed"' "$LAST_OUT" 2>/dev/null | grep '"agent_message"' | tail -1 | grep -o '"text":"[^"]*"' | tail -1 | sed 's/^"text":"//; s/"$//'
}

codex_add_invalid_model() {
  CMD+=(-m no-such-model-probe)
}

codex_set_invalid_credentials() {
  RUN_ENV=(OPENAI_API_KEY=invalid CODEX_HOME="$RUN_CWD/codex-home")
}

codex_judge_network() {
  if [ "$NATIVE_SANDBOX" = 1 ] && [ "$1" = allowed ]; then
    echo pass
  else
    echo "$2"
  fi
}

codex_write_project_context() {
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/AGENTS.md"
}

codex_skip_reason() {
  case "$1" in
    no-subagents) echo "codex option to disable subagents not determined (issue #4: 待查); not run" ;;
  esac
}

codex_judge_subagents() {
  local id=$1
  if [ -f "$RUN_CWD/sub1.txt" ] && [ -f "$RUN_CWD/sub2.txt" ]; then
    record "$id" codex pass "both files written; inspect stdout.jsonl for subagent events; $(codex_summary)"
  else
    record "$id" codex fail "files=$(workdir_files); model said: $(codex_result_text | head -c 160); $(codex_summary)"
  fi
}
