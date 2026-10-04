# shellcheck shell=bash disable=SC2034

CODEX_DISABLE="apps plugins remote_plugin plugin_sharing tool_suggest hooks skill_mcp_dependency_install image_generation goals memories shell_snapshot computer_use browser_use browser_use_external in_app_browser daemon_auto_start"
CODEX_MODEL_HOSTS="chatgpt.com ab.chatgpt.com auth.openai.com api.openai.com"
CODEX_INVALID_AUTH=0
CODEX_HOME_OVERRIDE=""
CODEX_HOME_DIR=""

codex_bin() {
  echo codex
}

codex_needs_preflight() {
  return 1
}

codex_drop_env_names() {
  local v
  for v in CODEX_API_KEY OPENAI_API_KEY OPENAI_BASE_URL; do
    env_names | grep -x "$v"
  done
}

codex_supports() {
  return 0
}

codex_prepare_home() {
  if [ -n "$CODEX_HOME_OVERRIDE" ]; then
    CODEX_HOME_DIR=$CODEX_HOME_OVERRIDE
  else
    CODEX_HOME_DIR="$PRIVATE_ROOT/codex-home-${CHECK_ID//\//-}"
    make_private_dirs "$CODEX_HOME_DIR"
    if [ "$CODEX_INVALID_AUTH" = 1 ]; then
      (umask 077 && printf '{"OPENAI_API_KEY":"invalid"}\n' > "$CODEX_HOME_DIR/auth.json")
      CODEX_INVALID_AUTH=0
    elif [ -n "$CODEX_AUTH_TARGET" ]; then
      ln -s "$CODEX_AUTH_TARGET" "$CODEX_HOME_DIR/auth.json"
    fi
  fi
  RUN_ENV+=("CODEX_HOME=$CODEX_HOME_DIR")
}

codex_auth_link_state() {
  local f="$CODEX_HOME_DIR/auth.json"
  if [ -z "$CODEX_AUTH_TARGET" ]; then echo none
  elif [ -L "$f" ]; then echo intact
  elif [ -e "$f" ]; then echo replaced-by-file
  else echo missing
  fi
}

codex_permission_args() {
  local mode=$1
  CODEX_PERM=(-c default_permissions=agentrun
    -c "permissions.agentrun.filesystem={\":root\"=\"read\", \":workspace_roots\"={\".\"=\"write\"}, \"$(real_dir "$SESSION_TMP")\"=\"write\"}")
  if [ "$mode" = proxy ]; then
    CODEX_PERM+=(--enable network_proxy
      -c "permissions.agentrun.network={enabled=true, enable_socks5=false, enable_socks5_udp=false, allow_upstream_proxy=true, domains={\"$NET_HOST\"=\"allow\"}}")
  else
    CODEX_PERM+=(-c permissions.agentrun.network.enabled=false)
  fi
}

codex_build_cmd() {
  local prompt=$1 mode=$2 f
  codex_prepare_home
  printf '%s' "$prompt" > "$CHECK_DIR/prompt.txt"
  STDIN_FILE="$CHECK_DIR/prompt.txt"
  CMD=(codex exec --json --skip-git-repo-check --ignore-user-config --ignore-rules -C "$RUN_CWD")
  if [ "$SANDBOX" = on ]; then
    codex_permission_args "$mode"
    CMD+=("${CODEX_PERM[@]}")
  else
    CMD+=(-c 'default_permissions=":danger-full-access"')
  fi
  CMD+=(-c approval_policy='"never"' -c web_search='"disabled"' -c project_doc_max_bytes=0
    -c skills.include_instructions=false -c skills.bundled.enabled=false -c allow_login_shell=false)
  for f in $CODEX_DISABLE; do
    CMD+=(--disable "$f")
  done
  if [ "$mode" = no-subagents ]; then
    CMD+=(-c agents.enabled=false --disable multi_agent --disable multi_agent_v2)
  fi
  CMD+=(-)
}

codex_events() {
  jsonl_query "$LAST_OUT" "$1"
}

codex_ok() {
  [ "$LAST_RC" = 0 ] && [ "$(codex_events 'return String(lines.some((l) => l.type === "turn.completed") && !lines.some((l) => l.type === "turn.failed" || l.type === "error"));')" = true ]
}

codex_item_types() {
  codex_events 'const c = {}; for (const l of lines) if (l.item) c[l.item.type] = (c[l.item.type] || 0) + 1; return Object.entries(c).map(([k, n]) => k + " x" + n).join(" ");'
}

codex_summary() {
  echo "exit=$LAST_RC auth_link=$(codex_auth_link_state) events: $(event_types) failed=$(codex_events 'const f = lines.filter((l) => l.type === "turn.failed" || l.type === "error").pop(); return f ? JSON.stringify((f.error && f.error.message) || f.message || "").slice(0, 160) : "none";') stderr: $(stderr_head 120)"
}

codex_result_text() {
  codex_events 'const m = lines.filter((l) => l.type === "item.completed" && l.item && l.item.type === "agent_message").pop(); return m ? m.item.text : undefined;'
}

codex_add_invalid_model() {
  CMD=("${CMD[@]:0:${#CMD[@]}-1}" -m no-such-model-probe -)
}

codex_add_invalid_arg() {
  CMD=("${CMD[@]:0:${#CMD[@]}-1}" --no-such-flag-probe -)
}

codex_set_invalid_credentials() {
  CODEX_INVALID_AUTH=1
}

codex_write_project_context() {
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/AGENTS.md"
}

codex_collab_items() {
  codex_events 'return String(lines.filter((l) => l.type === "item.completed" && l.item && l.item.type === "collab_tool_call").length);'
}

codex_parent_writes() {
  codex_events 'return String(lines.filter((l) => l.type === "item.completed" && l.item && (l.item.type === "command_execution" || l.item.type === "file_change")).length);'
}

codex_judge_subagents() {
  local name=$1
  if [ -f "$RUN_CWD/sub1.txt" ] && [ -f "$RUN_CWD/sub2.txt" ] && [ "$(codex_collab_items)" != 0 ] && [ "$(codex_parent_writes)" = 0 ]; then
    record "$name" codex pass "both files written; item types: $(codex_item_types); $(codex_summary)"
  elif ! codex_ok; then
    record "$name" codex unknown "runtime did not complete; files=$(workdir_files); $(codex_summary)"
  else
    record "$name" codex fail "files=$(workdir_files); item types: $(codex_item_types); model said: $(codex_result_text | head -c 160); $(codex_summary)"
  fi
}

codex_judge_no_subagents() {
  local name=$1
  if ! codex_ok; then
    record "$name" codex unknown "runtime did not complete; $(codex_summary)"
  elif [ "$(codex_collab_items)" = 0 ]; then
    record "$name" codex pass "no subagent items; files=$(workdir_files); item types: $(codex_item_types); model said: $(codex_result_text | head -c 120); $(codex_summary)"
  else
    record "$name" codex fail "subagent items present; item types: $(codex_item_types); $(codex_summary)"
  fi
}

codex_judge_network() {
  printf '%s' "$2"
}

check_codex_sandbox() {
  local id=codex-sandbox marker outside other home net
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  marker=$(rand_hex)
  outside="$HOME/agentrun-probe-outside-$marker"
  other="/tmp/agentrun-probe-$marker"
  home="$PRIVATE_ROOT/codex-sandbox-home"
  make_private_dirs "$home"
  RUN_ENV+=("CODEX_HOME=$home")
  codex_permission_args none
  run_capture stdout.txt codex sandbox -P agentrun "${CODEX_PERM[@]:2}" -C "$RUN_CWD" -- sh -c 'echo "TMPDIR=$TMPDIR"; for f in "$@"; do if echo probe > "$f" 2>/dev/null; then echo "written: $f"; else echo "blocked: $f"; fi; done
echo "direct: $(curl -sS -m 10 -o /dev/null -w "%{http_code}" "https://'"$NET_HOST"'/" 2>&1 | tr "\n" " ")"' sh "$RUN_CWD/inside.txt" "$SESSION_TMP/probe.txt" "$outside" "$other"
  local inside=missing tmp=missing h=blocked t=blocked seen
  [ -f "$RUN_CWD/inside.txt" ] && inside=written
  [ -f "$SESSION_TMP/probe.txt" ] && tmp=written
  [ -f "$outside" ] && h=written
  [ -f "$other" ] && t=written
  rm -f "$outside" "$other"
  seen=$(sed -n 's/^TMPDIR=//p' "$CHECK_DIR/stdout.txt")
  net=$(sed -n 's/^direct: //p' "$CHECK_DIR/stdout.txt")
  local detail="cwd=$inside session-tmp=$tmp \$HOME=$h /tmp=$t TMPDIR=[$seen] direct: $net"
  case "$net" in *200*) record $id codex fail "$detail"; end_check; return ;; esac
  if [ "$inside" = written ] && [ "$tmp" = written ] && [ "$h" = blocked ] && [ "$t" = blocked ]; then
    record $id codex pass "$detail"
  else
    record $id codex fail "$detail exit=$LAST_RC $(stderr_head)"
  fi
  end_check
}
