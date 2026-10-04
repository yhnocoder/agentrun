# shellcheck shell=bash disable=SC2034

PI_TOOLS="read,bash,edit,write,grep,find,ls"
PI_INVALID_AUTH=0

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

pi_prepare_agent_dir() {
  local dir="$SESSION_TMP/pi-agent"
  make_private_dirs "$dir"
  rm -f "$dir/auth.json" "$dir/models.json"
  if [ "$PI_INVALID_AUTH" = 1 ]; then
    (umask 077 && printf '{"%s":{"type":"api_key","key":"invalid"}}\n' "$PI_PROVIDER" > "$dir/auth.json")
    PI_INVALID_AUTH=0
  elif [ -n "$PI_AUTH_TARGET" ]; then
    ln -s "$PI_AUTH_TARGET" "$dir/auth.json"
  fi
  [ -f "$PI_STATE_DIR/models.json" ] && ln -s "$(real_file "$PI_STATE_DIR/models.json")" "$dir/models.json"
  for b in fd rg; do
    if [ -f "$PI_STATE_DIR/bin/$b" ]; then
      make_private_dirs "$dir/bin"
      ln -sfn "$(real_file "$PI_STATE_DIR/bin/$b")" "$dir/bin/$b"
    fi
  done
  node -e '
const fs = require("fs");
let s = {};
try { s = JSON.parse(fs.readFileSync(process.argv[1], "utf8")); } catch (e) {}
const out = {};
for (const k of ["defaultProvider", "defaultModel"]) if (s[k] !== undefined) out[k] = s[k];
fs.writeFileSync(process.argv[2], JSON.stringify(out, null, 2) + "\n", { mode: 0o600 });
' "$PI_STATE_DIR/settings.json" "$dir/settings.json"
  RUN_ENV+=("PI_CODING_AGENT_DIR=$dir")
}

pi_auth_link_state() {
  local f="$SESSION_TMP/pi-agent/auth.json"
  if [ -z "$PI_AUTH_TARGET" ]; then echo none
  elif [ -L "$f" ]; then echo intact
  elif [ -e "$f" ]; then echo replaced-by-file
  else echo missing
  fi
}

pi_build_cmd() {
  local prompt=$1 mode=$2
  pi_prepare_agent_dir
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
    echo "exit=$LAST_RC no assistant message_end; auth_link=$(pi_auth_link_state); events: $(event_types); stderr: $(stderr_head)"
    return
  fi
  echo "exit=$LAST_RC last_end=$info auth_link=$(pi_auth_link_state)"
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
  PI_INVALID_AUTH=1
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
