# crumb probe: prints the host-side facts crumb needs in one pass.
# Sent on stdin (`bash -s`), so nothing is installed on the host. crumb
# prepends PROJECT_PREFIX, DB_DOCKER, DB_URL, DB_USER, DB_LIKE, SCHEMA_QUERY,
# RUNTIME and READY assignments.
#
# Never print container environment variables: they hold secrets.
#
# Sections run in parallel; the slowest is usually Docker's own container list.

tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT

containers() {
  ids=$(docker ps -a --no-trunc --filter label=com.docker.compose.project \
    --format '{{.ID}} {{.Label "com.docker.compose.project"}}' |
    awk -v prefix="$PROJECT_PREFIX" 'index($2, prefix) == 1 { print $1 }')
  echo "@@containers"
  if [ -n "$ids" ]; then
    # shellcheck disable=SC2086
    docker inspect --format '{"id":{{json .Id}},"name":{{json .Name}},"status":{{json .State.Status}},"exit_code":{{.State.ExitCode}},"oom_killed":{{.State.OOMKilled}},"started_at":{{json .State.StartedAt}},"finished_at":{{json .State.FinishedAt}},"health":{{if .State.Health}}{{json .State.Health.Status}}{{else}}null{{end}},"restarts":{{.RestartCount}},"labels":{{json .Config.Labels}},"image":{{json .Image}},"mounts":{{json .Mounts}},"ports":{{json .NetworkSettings.Ports}},"bindings":{{json .HostConfig.PortBindings}}}' $ids
  fi
  echo "@@memory"
  for id in $ids; do
    f="/sys/fs/cgroup/system.slice/docker-$id.scope/memory.current"
    if [ -r "$f" ]; then echo "$id $(cat "$f")"; fi
  done
}

# psql on the configured server, in database $1.
psql_in() {
  if [ -n "${DB_DOCKER:-}" ]; then
    docker exec "$DB_DOCKER" psql -U "${DB_USER:-postgres}" -d "$1" -Atc "$2"
  elif [ -n "${DB_URL:-}" ]; then
    base="${DB_URL%%\?*}"
    params=""
    case "$DB_URL" in *\?*) params="?${DB_URL#*\?}" ;; esac
    case "${base#*://}" in */*) base="${base%/*}" ;; esac
    psql "$base/$1$params" -Atc "$2"
  fi
}

databases() {
  [ -n "${DB_DOCKER:-}${DB_URL:-}" ] || return 0
  query="select coalesce(json_agg(json_build_object('name', datname, 'comment', shobj_description(oid, 'pg_database')) order by datname), '[]') from pg_database where datname like '$DB_LIKE'"
  list=$(psql_in postgres "$query")
  echo "@@databases"
  echo "$list"
  [ -n "${SCHEMA_QUERY:-}" ] || return 0
  echo "@@schema"
  names=$(printf '%s' "$list" | grep -o '"name" *: *"[^"]*"' | sed 's/.*"\([^"]*\)"$/\1/')
  [ -n "$names" ] || return 0
  if [ -n "${DB_DOCKER:-}" ]; then
    # One docker exec for every database: each exec costs about 100 ms.
    # shellcheck disable=SC2086,SC2016
    docker exec -e Q="$SCHEMA_QUERY" -e U="${DB_USER:-postgres}" "$DB_DOCKER" sh -c \
      'for db in "$@"; do (echo "$db $(psql -U "$U" -d "$db" -Atc "$Q" 2>/dev/null | head -n 1)") & done; wait' \
      sh $names
  else
    for db in $names; do
      (echo "$db $(psql_in "$db" "$SCHEMA_QUERY" 2>/dev/null | head -n 1)") &
    done
    wait
  fi
}

# Process leases: tmux sessions named after the project, with the port and
# worktree `crumb up` stored in the session and the ready URL's answer.
sessions() {
  echo "@@tmux"
  command -v tmux >/dev/null 2>&1 || return 0
  tmux list-panes -a -F '#{session_name} #{session_created} #{pane_dead} #{pane_dead_status}' 2>/dev/null |
    while read -r name created dead status; do
      case "$name" in "$PROJECT_PREFIX"*) ;; *) continue ;; esac
      env=$(tmux show-environment -t "=$name" 2>/dev/null)
      port=$(printf '%s\n' "$env" | sed -n 's/^CRUMB_PORT=//p')
      worktree=$(printf '%s\n' "$env" | sed -n 's/^CRUMB_WORKTREE=//p')
      ready=-
      if [ -n "$READY" ] && [ -n "$port" ] && [ "$dead" = 0 ]; then
        url=$(printf '%s' "$READY" | sed -e "s/{port}/$port/g" -e "s/{local_port}/$port/g")
        if curl -fsS -m 1 -o /dev/null "$url" 2>/dev/null; then ready=1; else ready=0; fi
      fi
      echo "$name $created $dead ${status:-0} ${port:-0} $ready $worktree"
    done
}

system() {
  echo "@@mem"
  free -m 2>/dev/null | awk '/^Mem:/ { print $2, $7 }'
  echo "@@listeners"
  if command -v ss >/dev/null 2>&1; then
    ss -ltnH 2>/dev/null | awk '{ print $4 }'
  elif command -v lsof >/dev/null 2>&1; then
    lsof -nP -iTCP -sTCP:LISTEN 2>/dev/null | awk 'NR > 1 { print $9 }'
  fi
}

containers >"$tmp/containers" 2>"$tmp/containers.err" &
databases >"$tmp/databases" 2>"$tmp/databases.err" &
system >"$tmp/system" 2>/dev/null &
if [ "${RUNTIME:-}" = process ]; then
  sessions >"$tmp/sessions" 2>/dev/null &
else
  : >"$tmp/sessions"
fi
wait

echo "@@crumb-probe 1"
for part in system containers databases sessions; do
  cat "$tmp/$part"
done
echo "@@warnings"
cat "$tmp/containers.err" "$tmp/databases.err" 2>/dev/null
echo "@@end"
