# shellcheck shell=bash disable=SC2034

SEATBELT="$SCRIPT_DIR/seatbelt.sb"

sandbox_available() {
  case "$SANDBOX_KIND" in
    seatbelt) have sandbox-exec && [ -f "$SEATBELT" ] ;;
    bwrap) have bwrap ;;
  esac
}

sandbox_wrap() {
  local work=$1 tmp=$2 state=$3
  shift 3
  case "$SANDBOX_KIND" in
    seatbelt)
      SANDBOX_CMD=(sandbox-exec -f "$SEATBELT" -D "WORKDIR=$work" -D "PRIVTMP=$tmp" -D "STATEDIR=$state" "$@")
      ;;
    bwrap)
      SANDBOX_CMD=(bwrap --ro-bind / / --bind "$work" "$work" --bind "$tmp" "$tmp")
      if [ -d "$state" ] && [ "$state" != "$work" ]; then
        SANDBOX_CMD+=(--bind "$state" "$state")
      fi
      SANDBOX_CMD+=(--dev /dev --proc /proc --die-with-parent -- "$@")
      ;;
  esac
}

check_seatbelt_write_s1() {
  local id=S1 w marker
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "sandbox-exec or seatbelt.sb missing"
    return
  fi
  marker=$(rand_hex)
  w="$RUN_CWD"
  sandbox_wrap "$w" "$w" "$w" sh -c "echo in > '$w/inside.txt' && echo inside_write_ok; echo out > '$HOME/agentrun-probe-outside-$marker' && echo outside_write_ok || echo outside_write_failed"
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  if [ -f "$HOME/agentrun-probe-outside-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker"
    record $id - fail "outside write under \$HOME succeeded"
  elif [ -f "$w/inside.txt" ]; then
    record $id - pass "inside write ok, outside write blocked"
  else
    record $id - fail "inside write failed; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_bwrap_write_s2() {
  local id=S2 w marker
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "bwrap missing"
    return
  fi
  marker=$(rand_hex)
  w="$RUN_CWD"
  sandbox_wrap "$w" "$w" "$w" sh -c "echo in > '$w/inside.txt' && echo inside_write_ok; echo out > '$HOME/agentrun-probe-outside-$marker' && echo outside_write_ok || echo outside_write_failed; echo out > /usr/local/agentrun-probe-$marker && echo usr_local_write_ok || echo usr_local_write_failed"
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  rm -f "/usr/local/agentrun-probe-$marker"
  if [ -f "$HOME/agentrun-probe-outside-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker"
    record $id - fail "outside write under \$HOME succeeded"
  elif [ -f "$w/inside.txt" ]; then
    record $id - pass "inside write ok, outside write blocked"
  else
    record $id - fail "bwrap failed; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_bwrap_network_s3() {
  local id=S3 w proxy port sp blocked bridged
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! have bwrap; then
    record $id - unknown "bwrap missing"
    return
  fi
  w="$RUN_CWD"
  run_capture stdout.txt bwrap --ro-bind / / --bind "$w" "$w" --dev /dev --proc /proc --unshare-net --die-with-parent -- curl -sS -m 10 -o /dev/null -w '%{http_code}\n' "$NET_URL"
  mv "$CHECK_DIR/stdout.txt" "$CHECK_DIR/stdout.unshare-net.txt"
  mv "$CHECK_DIR/stderr.txt" "$CHECK_DIR/stderr.unshare-net.txt"
  mv "$CHECK_DIR/cmd.txt" "$CHECK_DIR/cmd.unshare-net.txt"
  blocked="exit=$LAST_RC code=$(head -1 "$CHECK_DIR/stdout.unshare-net.txt")"
  if [ "$LAST_RC" = 0 ]; then
    record $id - fail "curl succeeded with --unshare-net ($blocked)"
    return
  fi
  if ! have socat; then
    record $id - unknown "unshare-net blocks curl ($blocked); socat missing, bridge not tested"
    return
  fi
  proxy=${HTTPS_PROXY:-$https_proxy}
  proxy=${proxy#http://}
  proxy=${proxy%%/*}
  if [ -z "$proxy" ]; then
    record $id - unknown "unshare-net blocks curl ($blocked); HTTPS_PROXY unset, no proxy to bridge to"
    return
  fi
  port=$((20000 + $(od -An -N2 -tu2 /dev/urandom | tr -d ' ') % 20000))
  socat UNIX-LISTEN:"$w/proxy.sock",fork TCP:"$proxy" 2>"$CHECK_DIR/socat-outside.txt" &
  sp=$!
  sleep 1
  run_capture stdout.txt bwrap --ro-bind / / --bind "$w" "$w" --dev /dev --proc /proc --unshare-net --die-with-parent -- sh -c "socat TCP-LISTEN:$port,bind=127.0.0.1,fork,reuseaddr UNIX-CONNECT:$w/proxy.sock & s=\$!; sleep 1; curl -sS -m 15 -x http://127.0.0.1:$port -o /dev/null -w '%{http_code}\n' '$NET_URL'; rc=\$?; kill \$s; exit \$rc"
  kill "$sp" 2>/dev/null
  wait "$sp" 2>/dev/null
  bridged="exit=$LAST_RC code=$(head -1 "$CHECK_DIR/stdout.txt")"
  if [ "$LAST_RC" = 0 ]; then
    record $id - pass "unshare-net blocks curl ($blocked); via socat bridge to proxy ok ($bridged)"
  else
    record $id - fail "unshare-net blocks curl ($blocked); socat bridge failed ($bridged) $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_private_tmpdir_s4() {
  local id=S4 w priv marker tmp_other
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "no sandbox tool"
    return
  fi
  w="$RUN_CWD"
  priv="$RUN_CWD-tmp"
  mkdir -m 0700 "$priv"
  marker=$(rand_hex)
  sandbox_wrap "$w" "$priv" "$w" sh -c "echo t > '$priv/f.txt' && echo private_tmp_write_ok || echo private_tmp_write_failed; echo t > '/tmp/agentrun-probe-$marker' && echo tmp_other_write_ok || echo tmp_other_write_failed; echo t > '$BASE_TMP/agentrun-probe-$marker' && echo base_tmp_write_ok || echo base_tmp_write_failed"
  RUN_ENV=("TMPDIR=$priv")
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  tmp_other=$(grep -o 'tmp_other_write_[a-z]*\|base_tmp_write_[a-z]*' "$CHECK_DIR/stdout.txt" | tr '\n' ' ')
  rm -f "/tmp/agentrun-probe-$marker" "$BASE_TMP/agentrun-probe-$marker"
  if [ -f "$priv/f.txt" ]; then
    record $id - pass "private 0700 dir under \$TMPDIR writable; other: $tmp_other"
  else
    record $id - fail "private dir not writable; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}

check_child_process_s5() {
  local id=S5 w marker
  begin_check $id
  CHECK_TIMEOUT=$SANDBOX_TIMEOUT
  if ! sandbox_available; then
    record $id - unknown "no sandbox tool"
    return
  fi
  w="$RUN_CWD"
  marker=$(rand_hex)
  sandbox_wrap "$w" "$w" "$w" sh -c "/bin/sh -c \"echo c > '$w/child.txt' && echo child_inside_ok; echo c > '$HOME/agentrun-probe-outside-$marker' && echo child_outside_ok || echo child_outside_failed\"; env sh -c \"echo g > '$HOME/agentrun-probe-outside2-$marker' && echo grandchild_outside_ok || echo grandchild_outside_failed\""
  run_capture stdout.txt "${SANDBOX_CMD[@]}"
  if [ -f "$HOME/agentrun-probe-outside-$marker" ] || [ -f "$HOME/agentrun-probe-outside2-$marker" ]; then
    rm -f "$HOME/agentrun-probe-outside-$marker" "$HOME/agentrun-probe-outside2-$marker"
    record $id - fail "child process wrote outside the sandbox"
  elif [ -f "$w/child.txt" ]; then
    record $id - pass "child processes restricted: $(grep -o '[a-z_]*_ok\|[a-z_]*_failed' "$CHECK_DIR/stdout.txt" | tr '\n' ' ')"
  else
    record $id - fail "child inside write failed; exit=$LAST_RC; $(head -c 200 "$CHECK_DIR/stderr.txt" | tr '\n' ' ')"
  fi
}
