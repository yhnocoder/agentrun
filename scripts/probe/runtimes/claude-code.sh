# shellcheck shell=bash disable=SC2034

CLAUDE_TOOLS="Read,Edit,Write,Glob,Grep"
CLAUDE_SUBAGENT_TOOL="Task"
CLAUDE_SUBAGENT_TOOL_USE_NAMES='"name":"Task"\|"name":"Agent"'
CLAUDE_SANDBOX_SETTINGS='{"sandbox":{"enabled":true,"autoAllowBashIfSandboxed":true,"allowUnsandboxedCommands":false}}'

claude_code_bin() {
  echo claude
}

claude_code_build_cmd() {
  local prompt=$1 mode=$2
  CMD=(claude -p "$prompt" --output-format stream-json --verbose --session-id "$(new_uuid)" --no-session-persistence --permission-mode auto --permission-prompts none --setting-sources "" --strict-mcp-config)
  if [ "$mode" = no-subagents ]; then
    CMD+=(--allowedTools "$CLAUDE_TOOLS" --disallowedTools "$CLAUDE_SUBAGENT_TOOL")
  else
    CMD+=(--allowedTools "$CLAUDE_TOOLS,$CLAUDE_SUBAGENT_TOOL")
  fi
  if [ "$NATIVE_SANDBOX" = 1 ] || [ "$mode" = own-sandbox ]; then
    CMD+=(--settings "$CLAUDE_SANDBOX_SETTINGS")
  fi
}

claude_code_result_line() {
  grep '"type":"result"' "$LAST_OUT" 2>/dev/null | tail -1
}

claude_code_ok() {
  local line
  line=$(claude_code_result_line)
  [ "$LAST_RC" = 0 ] && [ -n "$line" ] && [ "$(json_raw "$line" is_error)" = false ]
}

claude_code_summary() {
  local line
  line=$(claude_code_result_line)
  if [ -z "$line" ]; then
    echo "no result event; exit=$LAST_RC; stderr: $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
    return
  fi
  echo "exit=$LAST_RC is_error=$(json_raw "$line" is_error) subtype=$(json_str "$line" subtype) terminal_reason=$(json_str "$line" terminal_reason) model=$(json_str "$(grep '"subtype":"init"' "$LAST_OUT" | head -1)" model) usage_keys=$(printf '%s' "$line" | sed 's/.*"usage":{//; s/,"modelUsage".*//' | grep -o '"[a-z_0-9]*":' | tr -d '":' | sort -u | tr '\n' ',') spawned=$(printf '%s' "$line" | grep -o '"subagent_stats":{"spawned":[0-9]*' | grep -o '[0-9]*$') denials=$(printf '%s' "$line" | grep -o '"permission_denials":\[[^]]*\]' | grep -o '"tool_name"' | wc -l | tr -d ' ')"
}

claude_code_result_text() {
  json_str "$(claude_code_result_line)" result
}

claude_code_add_invalid_model() {
  CMD+=(--model no-such-model-probe)
}

claude_code_set_invalid_credentials() {
  RUN_ENV=(ANTHROPIC_API_KEY=invalid CLAUDE_CODE_OAUTH_TOKEN=invalid)
}

claude_code_judge_network() {
  echo "$2"
}

claude_code_write_project_context() {
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/CLAUDE.md"
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/AGENTS.md"
}

claude_code_skip_reason() {
  :
}

claude_code_judge_subagents() {
  local id=$1 sub spawned
  sub=$(grep -c '"parent_tool_use_id":"' "$LAST_OUT")
  spawned=$(claude_code_result_line | grep -o '"subagent_stats":{"spawned":[0-9]*' | grep -o '[0-9]*$')
  if [ -f "$RUN_CWD/sub1.txt" ] && [ -f "$RUN_CWD/sub2.txt" ] && [ "${spawned:-0}" -ge 2 ]; then
    record "$id" claude-code pass "spawned=$spawned events_with_parent_tool_use_id=$sub by_type=$(claude_code_result_line | grep -o '"by_type":{[^}]*}' | head -c 120) subagent_tool_uses=$(grep -o "$CLAUDE_SUBAGENT_TOOL_USE_NAMES" "$LAST_OUT" | sort | uniq -c | tr -d '"' | awk '{printf "%s x%s ", $2, $1}') modelUsage_models=$(claude_code_result_line | grep -o '"modelUsage":{.*"permission_denials"' | grep -o '"[a-z0-9.-]*":{"inputTokens"' | tr -d '"{' | sed 's/:inputTokens//' | tr '\n' ',') ; $(claude_code_summary)"
  else
    record "$id" claude-code fail "spawned=${spawned:-?} files=$(workdir_files); $(claude_code_summary)"
  fi
}

claude_code_judge_no_subagents() {
  local id=$1 spawned uses
  spawned=$(claude_code_result_line | grep -o '"subagent_stats":{"spawned":[0-9]*' | grep -o '[0-9]*$')
  uses=$(grep -c "$CLAUDE_SUBAGENT_TOOL_USE_NAMES" "$LAST_OUT")
  if [ "${spawned:-0}" = 0 ] && [ "$uses" = 0 ]; then
    record "$id" claude-code pass "no subagent started: spawned=${spawned:-?} ${CLAUDE_SUBAGENT_TOOL}_tool_uses=$uses init_tools_has_$CLAUDE_SUBAGENT_TOOL=$(grep '"subtype":"init"' "$LAST_OUT" | grep -o '"tools":\[[^]]*\]' | grep -c "\"$CLAUDE_SUBAGENT_TOOL\"") model said: $(claude_code_result_text | head -c 120); $(claude_code_summary)"
  else
    record "$id" claude-code fail "subagent still started: spawned=${spawned:-?} tool_uses=$uses; $(claude_code_summary)"
  fi
}
