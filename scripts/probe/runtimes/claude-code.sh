# shellcheck shell=bash disable=SC2034

CLAUDE_MODEL="sonnet"
CLAUDE_TOOLS="Read,Edit,Write,Glob,Grep,Bash,Task"
CLAUDE_ALLOWED_TOOLS="Read,Edit,Write,Glob,Grep,Task"
CLAUDE_EXTRA_ALLOW_WRITE=""

claude_code_bin() {
  echo claude
}

claude_code_needs_preflight() {
  return 0
}

claude_code_drop_env_names() {
  env_names_with_prefix ANTHROPIC_ CLAUDE_CODE_USE_
}

claude_code_supports() {
  return 0
}

claude_code_settings() {
  node -e '
const [cwd, tmp, ...extra] = process.argv.slice(1);
process.stdout.write(JSON.stringify({ sandbox: {
  enabled: true, failIfUnavailable: true, autoAllowBashIfSandboxed: true, allowUnsandboxedCommands: false,
  filesystem: { allowWrite: [cwd, tmp, ...extra] }, network: { allowedDomains: [] } } }));
' "$RUN_CWD" "$SESSION_TMP" ${CLAUDE_EXTRA_ALLOW_WRITE:+"$CLAUDE_EXTRA_ALLOW_WRITE"}
}

claude_code_build_cmd() {
  local prompt=$1 mode=$2 tools=$CLAUDE_TOOLS allowed=$CLAUDE_ALLOWED_TOOLS permission=auto sandboxed=0
  if [ "$SANDBOX" = on ] || [ "$mode" = fail-if-unavailable ]; then
    sandboxed=1
    permission=dontAsk
    allowed="Read,Glob,Grep,Edit(/$RUN_CWD/**),Write(/$RUN_CWD/**),Edit(/$SESSION_TMP/**),Write(/$SESSION_TMP/**),Task"
  fi
  printf '%s' "$prompt" > "$CHECK_DIR/prompt.txt"
  node -e '
const fs = require("fs");
const content = fs.readFileSync(process.argv[1], "utf8");
process.stdout.write(JSON.stringify({ type: "user", message: { role: "user", content } }) + "\n");
' "$CHECK_DIR/prompt.txt" > "$CHECK_DIR/stdin.jsonl"
  STDIN_FILE="$CHECK_DIR/stdin.jsonl"
  case "$mode" in
    no-subagents)
      tools=$(printf '%s' "$tools" | sed 's/,Task$//')
      allowed=$(printf '%s' "$allowed" | sed 's/,Task$//')
      ;;
    write-only)
      tools=$(printf '%s' "$tools" | sed 's/,Bash//')
      ;;
  esac
  CMD=(claude -p --output-format stream-json --verbose --input-format stream-json --replay-user-messages \
    --permission-mode "$permission" --setting-sources "" --strict-mcp-config --no-session-persistence \
    --session-id "$(new_uuid)" --tools "$tools" --allowedTools "$allowed")
  if [ "$sandboxed" = 1 ]; then
    CMD+=(--settings "$(claude_code_settings)")
  fi
  CMD+=(--model "$CLAUDE_MODEL")
}

claude_code_result() {
  jsonl_query "$LAST_OUT" "const r = lines.filter((l) => l.type === 'result'); return r.length ? $1 : undefined;"
}

claude_code_ok() {
  [ "$LAST_RC" = 0 ] && [ "$(claude_code_result 'String(r[r.length - 1].is_error)')" = false ]
}

claude_code_result_text() {
  claude_code_result 'r[r.length - 1].result'
}

claude_code_replays() {
  jsonl_query "$LAST_OUT" 'return String(lines.filter((l) => l.type === "user" && l.isReplay).length);'
}

claude_code_summary() {
  local last
  last=$(claude_code_result 'JSON.stringify({is_error: r[r.length-1].is_error, subtype: r[r.length-1].subtype, num_turns: r[r.length-1].num_turns, results: r.length, denials: (r[r.length-1].permission_denials || []).map((d) => d.tool_name).join("+") || "none"})')
  if [ -z "$last" ]; then
    echo "exit=$LAST_RC no result event; stderr: $(stderr_head)"
    return
  fi
  echo "exit=$LAST_RC result=$last init_model=$(jsonl_query "$LAST_OUT" 'const i = lines.find((l) => l.type === "system" && l.subtype === "init"); return i ? i.model : "none";') replays=$(claude_code_replays)"
}

claude_code_add_invalid_model() {
  local i
  for i in "${!CMD[@]}"; do
    [ "${CMD[$i]}" = --model ] && CMD[i+1]=no-such-model-probe
  done
}

claude_code_add_invalid_arg() {
  CMD+=(--no-such-flag-probe)
}

claude_code_set_invalid_credentials() {
  RUN_ENV+=(CLAUDE_CODE_OAUTH_TOKEN=invalid)
}

claude_code_judge_network() {
  printf '%s' "$2"
}

claude_code_write_project_context() {
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/CLAUDE.md"
  printf 'The project code word is %s. Always mention it.\n' "$1" > "$RUN_CWD/AGENTS.md"
}

claude_code_subagent_stats() {
  jsonl_query "$LAST_OUT" '
let uses = 0, child = 0, notes = 0;
for (const l of lines) {
  if (l.type === "assistant" && l.message && Array.isArray(l.message.content))
    for (const b of l.message.content) if (b.type === "tool_use" && (b.name === "Agent" || b.name === "Task")) uses++;
  if (l.parent_tool_use_id) child++;
  if (l.type === "system" && l.subtype === "task_notification") notes++;
}
const r = lines.filter((l) => l.type === "result").pop();
const init = lines.find((l) => l.type === "system" && l.subtype === "init");
return JSON.stringify({ subagent_tool_uses: uses, events_with_parent: child, task_notifications: notes,
  spawned: r && r.subagent_stats ? r.subagent_stats.spawned : null,
  init_has_task: !!(init && init.tools && init.tools.includes("Task")) });
'
}

claude_code_judge_subagents() {
  local name=$1 stats
  stats=$(claude_code_subagent_stats)
  if [ -f "$RUN_CWD/sub1.txt" ] && [ -f "$RUN_CWD/sub2.txt" ] && printf '%s' "$stats" | grep -q '"subagent_tool_uses":[1-9]'; then
    record "$name" claude-code pass "both files written; $stats; $(claude_code_summary)"
  elif ! claude_code_ok; then
    record "$name" claude-code unknown "runtime did not complete; files=$(workdir_files); $stats; $(claude_code_summary)"
  else
    record "$name" claude-code fail "files=$(workdir_files); $stats; $(claude_code_summary)"
  fi
}

claude_code_judge_no_subagents() {
  local name=$1 stats
  stats=$(claude_code_subagent_stats)
  if ! claude_code_ok; then
    record "$name" claude-code unknown "runtime did not complete; $stats; $(claude_code_summary)"
  elif printf '%s' "$stats" | grep -q '"subagent_tool_uses":0,' && printf '%s' "$stats" | grep -q '"init_has_task":false'; then
    record "$name" claude-code pass "no subagent started; $stats; model said: $(claude_code_result_text | head -c 120); $(claude_code_summary)"
  else
    record "$name" claude-code fail "$stats; $(claude_code_summary)"
  fi
}

claude_code_replay_matches_prompt() {
  jsonl_query "$LAST_OUT" '
const fs = require("fs");
const want = fs.readFileSync(process.env.PROBE_PROMPT_FILE, "utf8");
const u = lines.find((l) => l.type === "user" && l.isReplay);
if (!u) return "no-replay";
const c = u.message && u.message.content;
const got = typeof c === "string" ? c : Array.isArray(c) ? c.filter((b) => b.type === "text").map((b) => b.text).join("") : "";
return got === want ? "same" : "different(" + got.length + " vs " + want.length + " chars)";
'
}

claude_code_judge_prompt() {
  local name=$1 marker=$2 same text
  same=$(PROBE_PROMPT_FILE="$CHECK_DIR/prompt.txt" claude_code_replay_matches_prompt)
  text=$(claude_code_result_text)
  if [ "$LAST_RC" = 0 ] && [ "$same" = same ] && printf '%s' "$text" | grep -q "$marker"; then
    record "$name" claude-code pass "replayed prompt identical ($(wc -c < "$CHECK_DIR/prompt.txt" | tr -d ' ') bytes); reply contains marker; $(claude_code_summary)"
  elif ! claude_code_ok; then
    record "$name" claude-code unknown "runtime did not complete; replay=$same; $(claude_code_summary)"
  else
    record "$name" claude-code fail "replay=$same reply=$(printf '%s' "$text" | head -c 100); $(claude_code_summary)"
  fi
}
