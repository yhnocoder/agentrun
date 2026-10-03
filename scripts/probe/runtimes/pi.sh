# shellcheck shell=bash disable=SC2034

PI_TOOLS="read,bash,edit,write,grep,find,ls"

pi_bin() {
  echo pi
}

pi_build_cmd() {
  local prompt=$1 mode=$2
  CMD=(pi -p --mode json --no-session --no-extensions --no-skills --no-prompt-templates --no-themes --no-context-files --offline --tools "$PI_TOOLS" "$prompt")
  if [ "$NATIVE_SANDBOX" = 1 ] || [ "$mode" = own-sandbox ]; then
    if sandbox_available; then
      sandbox_wrap "$RUN_CWD" "$RT_TMP" "$PI_STATE_DIR" "${CMD[@]}"
      CMD=("${SANDBOX_CMD[@]}")
    fi
  fi
}

pi_last_end() {
  grep '"type":"message_end"' "$LAST_OUT" 2>/dev/null | grep '"role":"assistant"' | tail -1
}

pi_ok() {
  local stop
  stop=$(json_str "$(pi_last_end)" stopReason)
  [ -n "$stop" ] && [ "$stop" != error ] && [ "$stop" != aborted ]
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

pi_add_invalid_model() {
  CMD+=(--model no-such-model-probe)
}

pi_set_invalid_credentials() {
  RUN_ENV=(DEEPSEEK_API_KEY=invalid ANTHROPIC_API_KEY=invalid OPENAI_API_KEY=invalid)
}

pi_judge_network() {
  if [ "$NATIVE_SANDBOX" = 1 ] && [ "$1" = allowed ]; then
    echo pass
  else
    echo "$2"
  fi
}

pi_write_project_context() {
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/CLAUDE.md"
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/AGENTS.md"
}

pi_skip_reason() {
  case "$1" in
    subagents|no-subagents) echo "pi has no built-in subagent tool; not run" ;;
  esac
}
