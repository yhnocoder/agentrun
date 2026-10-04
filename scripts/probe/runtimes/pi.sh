# shellcheck shell=bash disable=SC2034

PI_TOOLS="read,bash,edit,write,grep,find,ls"

pi_bin() {
  echo pi
}

pi_needs_preflight() {
  return 0
}

pi_drop_env_names() {
  :
}

pi_supports() {
  case "$1" in
    subagents|no-subagents) echo "pi has no subagent"; return 1 ;;
  esac
  if [ "$SANDBOX" = on ] && [ -z "$PI_MODEL_HOST" ]; then
    echo "pi provider [${PI_PROVIDER:-unset}] is not in the model host table"
    return 1
  fi
  return 0
}

pi_build_cmd() {
  local prompt=$1 mode=$2
  printf '%s' "$prompt" > "$CHECK_DIR/prompt.txt"
  STDIN_FILE=/dev/null
  CMD=(pi -p --mode json --no-session --no-extensions --no-skills --no-prompt-templates --no-themes \
    --no-context-files --no-approve --offline --tools "$PI_TOOLS" -- "$prompt")
  if [ "$SANDBOX" = on ]; then
    start_proxy "$PI_MODEL_HOST" || return 1
    proxy_env
    wrap_cmd "${CMD[@]}"
    CMD=("${WRAPPED[@]}")
  fi
}

pi_last_end() {
  jsonl_query "$LAST_OUT" "const e = lines.filter((l) => l.type === 'message_end' && l.message && l.message.role === 'assistant'); return e.length ? $1 : undefined;"
}

pi_ok() {
  local stop
  stop=$(pi_last_end 'e[e.length - 1].message.stopReason')
  [ "$LAST_RC" = 0 ] && [ -n "$stop" ] && [ "$stop" != error ] && [ "$stop" != aborted ]
}

pi_summary() {
  local info
  info=$(pi_last_end 'JSON.stringify({stopReason: e[e.length-1].message.stopReason, model: e[e.length-1].message.provider + "/" + e[e.length-1].message.model, ends: e.length, errorMessage: (e[e.length-1].message.errorMessage || "").slice(0, 160) || undefined})')
  if [ -z "$info" ]; then
    echo "exit=$LAST_RC no assistant message_end; events: $(event_types); stderr: $(stderr_head)"
    return
  fi
  echo "exit=$LAST_RC last_end=$info"
}

pi_result_text() {
  pi_last_end 'e.map((m) => (Array.isArray(m.message.content) ? m.message.content : []).filter((b) => b.type === "text" && b.text).map((b) => b.text).pop()).filter(Boolean).pop()'
}

pi_insert_options() {
  local i
  for i in "${!CMD[@]}"; do
    if [ "${CMD[$i]}" = -p ] && [ "${CMD[$((i + 1))]}" = --mode ]; then
      CMD=("${CMD[@]:0:$((i + 1))}" "$@" "${CMD[@]:$((i + 1))}")
      return
    fi
  done
}

pi_add_invalid_model() {
  pi_insert_options --model deepseek/no-such-model-probe
}

pi_add_invalid_arg() {
  pi_insert_options --no-such-flag-probe
}

pi_set_invalid_credentials() {
  RUN_ENV+=(DEEPSEEK_API_KEY=invalid ANTHROPIC_API_KEY=invalid OPENAI_API_KEY=invalid)
}

pi_write_project_context() {
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/CLAUDE.md"
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/AGENTS.md"
}

pi_judge_network() {
  local state=$1 verdict=$2
  if [ "$SANDBOX" = on ]; then
    if proxy_log_has deny "$NET_HOST"; then
      [ "$state" = blocked ] && verdict=pass
      printf '%s' "$verdict"
    else
      [ "$verdict" = pass ] && verdict=unknown
      printf '%s' "$verdict"
    fi
  else
    printf '%s' "$verdict"
  fi
}
