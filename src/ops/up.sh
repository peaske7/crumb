# crumb up, host side: pick the lease's port and start its runtime, under one
# lock so two `up`s never take the same port. Sent on stdin (`bash -s`).
#
# crumb prepends: LEASE PROJECT RUNTIME STATE_ROOT PROJECT_DIR COMPOSE_FILE
# ENV_FILE SERVICE UP_ARGS START HOST_BASE LOCAL_BASE LOCAL_USED MEMORY_MB
# KEEP_FREE_MB and the functions write_compose and write_labels, which write
# the compose file and the labels override (with @@PORT@@ for the port).
#
# Prints "@@port <n>" once the port is known; progress goes to stderr.

set -eu

expand() {
  case "$1" in
    "~") printf '%s' "$HOME" ;;
    "~/"*) printf '%s/%s' "$HOME" "${1#\~/}" ;;
    *) printf '%s' "$1" ;;
  esac
}

STATE_ROOT=$(expand "$STATE_ROOT")
PROJECT_DIR=$(expand "$PROJECT_DIR")
STATE="$STATE_ROOT/$LEASE"
mkdir -p "$STATE"

lock="$STATE_ROOT/lock"
if command -v flock >/dev/null 2>&1; then
  exec 9>"$lock"
  flock -w 300 9 || { echo "another crumb up held $lock for 5 minutes" >&2; exit 1; }
else
  tries=0
  until mkdir "$lock.d" 2>/dev/null; do
    tries=$((tries + 1))
    [ "$tries" -lt 1500 ] || { echo "another crumb up held $lock.d for 5 minutes" >&2; exit 1; }
    sleep 0.2
  done
  trap 'rmdir "$lock.d"' EXIT
fi

listeners() {
  if command -v ss >/dev/null 2>&1; then
    ss -ltnH 2>/dev/null | awk '{ n = split($4, a, ":"); print a[n] }'
  elif command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR > 1 { n = split($9, a, ":"); print a[n] }'
  fi
}

in_range() { [ "$1" -gt "$HOST_BASE" ] && [ "$1" -lt $((HOST_BASE + 100)) ]; }

bindings='{{index .Config.Labels "crumb.port"}} {{range $k, $v := .HostConfig.PortBindings}}{{range $v}}{{.HostPort}} {{end}}{{end}}'

# The port this lease already has: its label, a published port in range (a
# lease an older tool started), or its tmux session's.
port=""
if [ "$RUNTIME" = compose ]; then
  ids=$(docker ps -aq --filter "label=com.docker.compose.project=$PROJECT")
  if [ -n "$ids" ]; then
    # shellcheck disable=SC2086
    for p in $(docker inspect --format "$bindings" $ids); do
      if in_range "$p"; then port=$p; break; fi
    done
  fi
elif tmux has-session -t "=$PROJECT" 2>/dev/null; then
  port=$(tmux show-environment -t "=$PROJECT" CRUMB_PORT 2>/dev/null | sed -n 's/^CRUMB_PORT=//p')
fi

if [ -z "$port" ]; then
  if [ -n "$MEMORY_MB" ] && [ -n "$KEEP_FREE_MB" ] && command -v free >/dev/null 2>&1; then
    available=$(free -m | awk '/^Mem:/ { print $7 }')
    if [ $((available - MEMORY_MB)) -lt "$KEEP_FREE_MB" ]; then
      echo "not enough memory: ${available} MB available, a lease needs ${MEMORY_MB} MB and ${KEEP_FREE_MB} MB must stay free" >&2
      exit 4
    fi
  fi
  used=$(
    {
      all=$(docker ps -aq 2>/dev/null || true)
      # shellcheck disable=SC2086
      [ -z "$all" ] || docker inspect --format "$bindings" $all
      if command -v tmux >/dev/null 2>&1; then
        for s in $(tmux list-sessions -F '#{session_name}' 2>/dev/null); do
          tmux show-environment -t "=$s" CRUMB_PORT 2>/dev/null | sed -n 's/^CRUMB_PORT=//p'
        done
      fi
      listeners
    } | tr ' ' '\n' | grep -E '^[0-9]+$' | sort -un
  )
  nn=1
  while [ "$nn" -le 99 ]; do
    p=$((HOST_BASE + nn))
    lp=$((LOCAL_BASE + nn))
    if ! printf '%s\n' "$used" | grep -qx "$p" && ! printf ',%s,' "$LOCAL_USED" | grep -q ",$lp,"; then
      port=$p
      break
    fi
    nn=$((nn + 1))
  done
  [ -n "$port" ] || { echo "no free port from $((HOST_BASE + 1)) to $((HOST_BASE + 99))" >&2; exit 3; }
fi
echo "@@port $port"

export CRUMB_LEASE="$LEASE" CRUMB_PORT="$port" CRUMB_DATABASE="${DATABASE:-}" CRUMB_WORKTREE="$WORKTREE"

if [ "$RUNTIME" = compose ]; then
  if [ -z "$COMPOSE_FILE" ]; then
    COMPOSE_FILE="$STATE/compose.yml"
    write_compose >"$COMPOSE_FILE"
  fi
  write_labels | sed "s/@@PORT@@/$port/" >"$STATE/labels.yml"
  set -- -p "$PROJECT" -f "$(expand "$COMPOSE_FILE")" -f "$STATE/labels.yml" --project-directory "$PROJECT_DIR"
  [ -z "$ENV_FILE" ] || set -- "$@" --env-file "$(expand "$ENV_FILE")"
  cd "$PROJECT_DIR"
  # shellcheck disable=SC2086
  docker compose "$@" up -d $UP_ARGS >&2
else
  if tmux has-session -t "=$PROJECT" 2>/dev/null; then
    if [ "$(tmux display-message -p -t "=$PROJECT" '#{pane_dead}')" = 1 ]; then
      tmux kill-session -t "=$PROJECT"
    else
      echo "already running in tmux session $PROJECT" >&2
      exit 0
    fi
  fi
  START=$(printf '%s' "$START" | sed -e "s/{port}/$port/g" -e "s/{local_port}/$port/g")
  log="$STATE/output.log"
  : >"$log"
  tmux new-session -d -s "$PROJECT" -c "$PROJECT_DIR" \
    -e "CRUMB_LEASE=$LEASE" -e "CRUMB_PORT=$port" -e "CRUMB_DATABASE=${DATABASE:-}" -e "CRUMB_WORKTREE=$WORKTREE" \
    "$START" \; set-option -t "=$PROJECT" remain-on-exit on \; pipe-pane -t "=$PROJECT" -o "cat >>'$log'"
  tmux set-environment -t "=$PROJECT" CRUMB_PORT "$port"
  echo "started tmux session $PROJECT" >&2
fi
