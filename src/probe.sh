# crumb probe: prints the host-side facts crumb needs in one pass.
# Sent on stdin (`bash -s`), so nothing is installed on the host. crumb
# prepends PROJECT_PREFIX, DB_DOCKER, DB_URL, DB_USER and DB_LIKE assignments.
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
    docker inspect --format '{"id":{{json .Id}},"name":{{json .Name}},"status":{{json .State.Status}},"exit_code":{{.State.ExitCode}},"started_at":{{json .State.StartedAt}},"finished_at":{{json .State.FinishedAt}},"health":{{if .State.Health}}{{json .State.Health.Status}}{{else}}null{{end}},"restarts":{{.RestartCount}},"labels":{{json .Config.Labels}},"image":{{json .Image}},"mounts":{{json .Mounts}},"ports":{{json .NetworkSettings.Ports}},"bindings":{{json .HostConfig.PortBindings}}}' $ids
  fi
  echo "@@memory"
  for id in $ids; do
    f="/sys/fs/cgroup/system.slice/docker-$id.scope/memory.current"
    if [ -r "$f" ]; then echo "$id $(cat "$f")"; fi
  done
}

databases() {
  echo "@@databases"
  query="select coalesce(json_agg(json_build_object('name', datname, 'comment', shobj_description(oid, 'pg_database')) order by datname), '[]') from pg_database where datname like '$DB_LIKE'"
  if [ -n "${DB_DOCKER:-}" ]; then
    docker exec "$DB_DOCKER" psql -U "${DB_USER:-postgres}" -d postgres -Atc "$query"
  elif [ -n "${DB_URL:-}" ]; then
    psql "$DB_URL" -Atc "$query"
  fi
}

system() {
  echo "@@mem"
  free -m 2>/dev/null | awk '/^Mem:/ { print $2, $7 }'
  echo "@@listeners"
  if command -v ss >/dev/null 2>&1; then
    ss -ltnH 2>/dev/null | awk '{ print $4 }'
  fi
}

containers >"$tmp/containers" 2>"$tmp/containers.err" &
databases >"$tmp/databases" 2>"$tmp/databases.err" &
system >"$tmp/system" 2>/dev/null &
wait

echo "@@crumb-probe 1"
for part in system containers databases; do
  cat "$tmp/$part"
done
echo "@@warnings"
cat "$tmp/containers.err" "$tmp/databases.err" 2>/dev/null
echo "@@end"
