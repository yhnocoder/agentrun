# shellcheck shell=bash disable=SC2034,SC2016

PREFLIGHT_OK=1
PREFLIGHT_DETAIL=""
WRAPPED=()
PROXY_PORT=""
PROXY_URL=""
PROXY_PIDS=""
PROXY_LOG=""

check_sandbox_preflight() {
  local id=sandbox-preflight missing="" p
  begin_check $id
  CHECK_TIMEOUT=$PREFLIGHT_TIMEOUT
  case "$SANDBOX_KIND" in
    seatbelt)
      if [ -x /usr/bin/sandbox-exec ]; then
        printf '0\n' > "$CHECK_DIR/exit_code"
        record $id - pass "/usr/bin/sandbox-exec is executable"
      else
        PREFLIGHT_OK=0
        PREFLIGHT_DETAIL="/usr/bin/sandbox-exec missing or not executable"
        printf '1\n' > "$CHECK_DIR/exit_code"
        record $id - fail "$PREFLIGHT_DETAIL"
      fi
      ;;
    bwrap)
      for p in bwrap socat; do
        have "$p" || missing="$missing $p"
      done
      if [ -n "$missing" ]; then
        PREFLIGHT_OK=0
        PREFLIGHT_DETAIL="missing in PATH:$missing"
        printf '1\n' > "$CHECK_DIR/exit_code"
        record $id - fail "$PREFLIGHT_DETAIL"
      else
        run_capture stdout.txt bwrap --ro-bind / / --dev /dev --proc /proc --die-with-parent -- /bin/true
        if [ "$LAST_RC" = 0 ] && [ "$LAST_TIMED_OUT" = 0 ]; then
          record $id - pass "bwrap started /bin/true in $(cat "$CHECK_DIR/duration_ms")ms"
        else
          PREFLIGHT_OK=0
          PREFLIGHT_DETAIL="bwrap exit=$LAST_RC: $(stderr_head 300)"
          LAST_TIMED_OUT=0
          record $id - fail "$PREFLIGHT_DETAIL"
        fi
      fi
      ;;
  esac
  end_check
}

ensure_pi_state_dir() {
  [ -d "$PI_STATE_DIR" ] || make_private_dirs "$PI_STATE_DIR"
}

path_is_under() {
  case "$1" in
    "$2"|"$2"/*) return 0 ;;
  esac
  return 1
}

write_seatbelt_profile() {
  local file=$1 cwd=$2 tmp=$3 state=$4 port=$5
  {
    echo '(version 1)'
    echo '(allow default)'
    echo '(deny file-write*)'
    echo '(allow file-write*'
    printf '  (subpath "%s")\n' "$cwd" "$tmp" "$state"
    echo '  (literal "/dev/null")'
    echo '  (literal "/dev/zero")'
    echo '  (literal "/dev/tty")'
    echo '  (regex #"^/dev/ttys[0-9]+$")'
    echo '  (literal "/dev/dtracehelper"))'
    echo '(deny network*)'
    if [ -n "$port" ]; then
      printf '(allow network-outbound (remote tcp "localhost:%s"))\n' "$port"
    fi
  } > "$file"
}

wrap_cmd() {
  local cwd state tmp
  cwd=$(real_dir "$RUN_CWD")
  tmp=$(real_dir "$SESSION_TMP")
  ensure_pi_state_dir
  state=$(real_dir "$PI_STATE_DIR")
  case "$SANDBOX_KIND" in
    seatbelt)
      write_seatbelt_profile "$SESSION_TMP/seatbelt.sb" "$cwd" "$tmp" "$state" "$PROXY_PORT"
      WRAPPED=(/usr/bin/sandbox-exec -f "$SESSION_TMP/seatbelt.sb" "$@")
      ;;
    bwrap)
      WRAPPED=(bwrap --ro-bind / / --bind "$RUN_CWD" "$RUN_CWD" --bind "$SESSION_TMP" "$SESSION_TMP")
      if ! path_is_under "$state" "$cwd"; then
        WRAPPED+=(--bind "$PI_STATE_DIR" "$PI_STATE_DIR")
      fi
      WRAPPED+=(--dev /dev --proc /proc --die-with-parent --unshare-net --)
      if [ -n "$PROXY_PORT" ]; then
        WRAPPED+=(sh -c 'socat "TCP-LISTEN:$0,bind=127.0.0.1,fork,reuseaddr" "UNIX-CONNECT:$1" & shift; exec "$@"' "$PROXY_PORT" "$SESSION_TMP/proxy.sock" "$@")
      else
        WRAPPED+=("$@")
      fi
      ;;
  esac
}

start_proxy() {
  local portfile="$SESSION_TMP/proxy.port" i pid
  PROXY_LOG="$SESSION_TMP/proxy.log"
  : > "$PROXY_LOG"
  node "$SCRIPT_DIR/filter-proxy.js" "$portfile" "$PROXY_LOG" "$@" 2>>"$CHECK_DIR/proxy-stderr.txt" &
  pid=$!
  PROXY_PIDS="$pid"
  i=0
  while [ ! -s "$portfile" ] && [ $i -lt 50 ]; do
    sleep 0.1
    i=$((i + 1))
  done
  if [ ! -s "$portfile" ]; then
    echo "filter-proxy did not report a port" >> "$CHECK_DIR/proxy-stderr.txt"
    return 1
  fi
  PROXY_PORT=$(cat "$portfile")
  PROXY_URL="http://127.0.0.1:$PROXY_PORT"
  if [ "$SANDBOX_KIND" = bwrap ]; then
    socat "UNIX-LISTEN:$SESSION_TMP/proxy.sock,fork" "TCP:127.0.0.1:$PROXY_PORT" 2>>"$CHECK_DIR/proxy-stderr.txt" &
    PROXY_PIDS="$PROXY_PIDS $!"
    i=0
    while [ ! -S "$SESSION_TMP/proxy.sock" ] && [ $i -lt 50 ]; do
      sleep 0.1
      i=$((i + 1))
    done
  fi
  return 0
}

proxy_env() {
  RUN_ENV+=("HTTPS_PROXY=$PROXY_URL" "HTTP_PROXY=$PROXY_URL" "ALL_PROXY=$PROXY_URL" "NO_PROXY=" \
    "https_proxy=$PROXY_URL" "http_proxy=$PROXY_URL" "all_proxy=$PROXY_URL" "no_proxy=")
}

stop_proxy() {
  local p
  for p in $PROXY_PIDS; do
    kill "$p" 2>/dev/null
    wait "$p" 2>/dev/null
  done
  PROXY_PIDS=""
  if [ -n "$PROXY_LOG" ] && [ -f "$PROXY_LOG" ] && [ -n "$CHECK_DIR" ]; then
    cp "$PROXY_LOG" "$CHECK_DIR/proxy.log"
  fi
  PROXY_LOG=""
  PROXY_PORT=""
  PROXY_URL=""
}

proxy_log_has() {
  [ -f "$CHECK_DIR/proxy.log" ] && grep -q " $1 $2:" "$CHECK_DIR/proxy.log"
}

sandbox_unavailable() {
  skip_check "$1" "$1" - "sandbox cannot start: $PREFLIGHT_DETAIL"
}

check_sandbox_write() {
  local id=sandbox-write marker outside usrlocal
  if [ "$PREFLIGHT_OK" = 0 ]; then sandbox_unavailable $id; return; fi
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  marker=$(rand_hex)
  outside="$HOME/agentrun-probe-outside-$marker"
  usrlocal="/usr/local/agentrun-probe-$marker"
  wrap_cmd sh -c 'for f in "$@"; do if echo probe > "$f" 2>/dev/null; then echo "written: $f"; else echo "blocked: $f"; fi; done' sh "$RUN_CWD/inside.txt" "$outside" "$usrlocal"
  run_capture stdout.txt "${WRAPPED[@]}"
  local inside=missing home=blocked ul=blocked
  [ -f "$RUN_CWD/inside.txt" ] && inside=written
  [ -f "$outside" ] && home=written
  [ -f "$usrlocal" ] && ul=written
  rm -f "$outside" "$usrlocal"
  if [ "$inside" = written ] && [ "$home" = blocked ] && [ "$ul" = blocked ]; then
    record $id - pass "cwd=$inside \$HOME=$home /usr/local=$ul"
  else
    record $id - fail "cwd=$inside \$HOME=$home /usr/local=$ul exit=$LAST_RC $(stderr_head)"
  fi
  end_check
}

check_sandbox_tmpdir() {
  local id=sandbox-tmpdir marker other seen tmp_state other_state
  if [ "$PREFLIGHT_OK" = 0 ]; then sandbox_unavailable $id; return; fi
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  marker=$(rand_hex)
  other="/tmp/agentrun-probe-$marker"
  wrap_cmd sh -c 'echo "TMPDIR=$TMPDIR"; for f in "$@"; do if echo probe > "$f" 2>/dev/null; then echo "written: $f"; else echo "blocked: $f"; fi; done' sh "$SESSION_TMP/probe.txt" "$other"
  run_capture stdout.txt "${WRAPPED[@]}"
  tmp_state=blocked; other_state=blocked
  [ -f "$SESSION_TMP/probe.txt" ] && tmp_state=written
  [ -f "$other" ] && other_state=written
  rm -f "$other"
  seen=$(grep '^TMPDIR=' "$CHECK_DIR/stdout.txt" | head -1 | sed 's/^TMPDIR=//')
  if [ "$tmp_state" = written ] && [ "$other_state" = blocked ] && [ "$seen" = "$SESSION_TMP" ]; then
    record $id - pass "session tmp=$tmp_state /tmp=$other_state TMPDIR inside sandbox equals session tmp"
  else
    record $id - fail "session tmp=$tmp_state /tmp=$other_state TMPDIR inside=[$seen] expected=[$SESSION_TMP] exit=$LAST_RC $(stderr_head)"
  fi
  end_check
}

check_sandbox_child_process() {
  local id=sandbox-child-process marker child grandchild c g
  if [ "$PREFLIGHT_OK" = 0 ]; then sandbox_unavailable $id; return; fi
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  marker=$(rand_hex)
  child="$HOME/agentrun-probe-child-$marker"
  grandchild="$HOME/agentrun-probe-grandchild-$marker"
  wrap_cmd sh -c '/bin/sh -c "echo c > \"$0\" 2>/dev/null && echo child_written || echo child_blocked"; /bin/sh -c "env sh -c \"echo g > $1 2>/dev/null && echo grandchild_written || echo grandchild_blocked\""' "$child" "$grandchild"
  run_capture stdout.txt "${WRAPPED[@]}"
  c=blocked; g=blocked
  [ -f "$child" ] && c=written
  [ -f "$grandchild" ] && g=written
  rm -f "$child" "$grandchild"
  if [ "$c" = blocked ] && [ "$g" = blocked ] && [ "$LAST_RC" = 0 ]; then
    record $id - pass "child=$c grandchild=$g output: $(tr '\n' ' ' < "$CHECK_DIR/stdout.txt")"
  else
    record $id - fail "child=$c grandchild=$g exit=$LAST_RC $(stderr_head)"
  fi
  end_check
}

check_sandbox_network() {
  local id=sandbox-network direct allowed denied log_allow log_deny
  if [ "$PREFLIGHT_OK" = 0 ]; then sandbox_unavailable $id; return; fi
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! start_proxy "$NET_HOST"; then
    record $id - unknown "filter-proxy failed to start: $(head -c 200 "$CHECK_DIR/proxy-stderr.txt" | tr '\n' ' ')"
    end_check
    return
  fi
  wrap_cmd sh -c 'i=0; while [ $i -lt 30 ] && ! curl -s -m 1 -o /dev/null "$0/" 2>/dev/null; do sleep 0.1; i=$((i+1)); done
echo "direct: $(curl -sS -m 10 --noproxy "*" -o /dev/null -w "%{http_code}" "https://$1/" 2>&1 | tr "\n" " ")"
echo "proxied-allowed: $(curl -sS -m 20 -x "$0" -o /dev/null -w "%{http_code}" "https://$1/" 2>&1 | tr "\n" " ")"
echo "proxied-denied: $(curl -sS -m 20 -x "$0" -o /dev/null -w "%{http_code}" "https://$2/" 2>&1 | tr "\n" " ")"' "$PROXY_URL" "$NET_HOST" "$DENIED_HOST"
  run_capture stdout.txt "${WRAPPED[@]}"
  stop_proxy
  direct=$(sed -n 's/^direct: //p' "$CHECK_DIR/stdout.txt")
  allowed=$(sed -n 's/^proxied-allowed: //p' "$CHECK_DIR/stdout.txt")
  denied=$(sed -n 's/^proxied-denied: //p' "$CHECK_DIR/stdout.txt")
  log_allow=0; log_deny=0
  proxy_log_has allow "$NET_HOST" && log_allow=1
  proxy_log_has deny "$DENIED_HOST" && log_deny=1
  case "$direct" in
    *200*) record $id - fail "direct curl reached $NET_HOST inside the sandbox: $direct"; end_check; return ;;
  esac
  if [ "$allowed" = 200 ] && [ "$denied" != 200 ] && [ "$log_allow" = 1 ] && [ "$log_deny" = 1 ]; then
    record $id - pass "direct: $direct; via proxy $NET_HOST: $allowed; via proxy $DENIED_HOST: $denied; proxy.log has allow and deny lines"
  else
    record $id - fail "direct: $direct; via proxy $NET_HOST: $allowed; via proxy $DENIED_HOST: $denied; proxy.log allow=$log_allow deny=$log_deny $(stderr_head)"
  fi
  end_check
}

run_sandbox_checks() {
  selected sandbox-write && check_sandbox_write
  selected sandbox-tmpdir && check_sandbox_tmpdir
  selected sandbox-child-process && check_sandbox_child_process
  selected sandbox-network && check_sandbox_network
}
