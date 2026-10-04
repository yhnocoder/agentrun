# shellcheck shell=bash disable=SC2034

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
  case "$1" in
    no-subagents) echo "codex option to disable subagents not yet determined (#5, #6)"; return 1 ;;
  esac
  return 0
}

codex_build_cmd() {
  local prompt=$1 mode=$2 network=false
  printf '%s' "$prompt" > "$CHECK_DIR/prompt.txt"
  STDIN_FILE="$CHECK_DIR/prompt.txt"
  [ "$mode" = proxy ] && network=true
  CMD=(codex exec --json --skip-git-repo-check --ephemeral)
  if [ "$SANDBOX" = on ]; then
    CMD+=(--sandbox workspace-write -c approval_policy='"never"' \
      -c "sandbox_workspace_write.network_access=$network" \
      -c sandbox_workspace_write.exclude_slash_tmp=true \
      -c sandbox_workspace_write.exclude_tmpdir_env_var=true \
      -c "sandbox_workspace_write.writable_roots=[\"$SESSION_TMP\"]")
  else
    CMD+=(--sandbox danger-full-access -c approval_policy='"never"')
  fi
  CMD+=(-c project_doc_max_bytes=0 -c allow_login_shell=false -c web_search='"disabled"' -)
}

codex_events() {
  jsonl_query "$LAST_OUT" "$1"
}

codex_ok() {
  [ "$LAST_RC" = 0 ] && [ "$(codex_events 'return String(lines.some((l) => l.type === "turn.completed") && !lines.some((l) => l.type === "turn.failed" || l.type === "error"));')" = true ]
}

codex_summary() {
  echo "exit=$LAST_RC events: $(event_types) failed=$(codex_events 'const f = lines.filter((l) => l.type === "turn.failed" || l.type === "error").pop(); return f ? JSON.stringify((f.error && f.error.message) || f.message || "").slice(0, 160) : "none";') stderr: $(stderr_head 120)"
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
  local home="$SESSION_TMP/codex-home-invalid"
  make_private_dirs "$home"
  (umask 077 && printf '{"OPENAI_API_KEY":"invalid"}\n' > "$home/auth.json")
  RUN_ENV+=("CODEX_HOME=$home")
}

codex_write_project_context() {
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/AGENTS.md"
}

codex_judge_subagents() {
  local name=$1
  if [ -f "$RUN_CWD/sub1.txt" ] && [ -f "$RUN_CWD/sub2.txt" ]; then
    record "$name" codex pass "both files written; item types: $(codex_events 'const c = {}; for (const l of lines) if (l.item) c[l.item.type] = (c[l.item.type] || 0) + 1; return Object.entries(c).map(([k, n]) => k + " x" + n).join(" ");'); $(codex_summary)"
  elif ! codex_ok; then
    record "$name" codex unknown "runtime did not complete; files=$(workdir_files); $(codex_summary)"
  else
    record "$name" codex fail "files=$(workdir_files); model said: $(codex_result_text | head -c 160); $(codex_summary)"
  fi
}

codex_judge_no_subagents() {
  record "$1" codex unknown "not run"
}

codex_judge_network() {
  printf '%s' "$2"
}

codex_user_auth_file() {
  echo "${CODEX_HOME:-$HOME/.codex}/auth.json"
}
