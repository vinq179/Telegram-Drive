#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

revision="${1:-}"
if [[ -z "$revision" ]]; then
  echo "usage: ./deploy.sh <git-commit> [env-file]" >&2
  exit 2
fi

env_file="${2:-/home/vinq179/runtime/secrets/telegram-drive.env}"

if [[ -n "$(git -C .. status --porcelain)" ]]; then
  echo "[deploy] Refusing dirty Telegram-Drive worktree." >&2
  exit 1
fi

if [[ ! -f "$env_file" ]]; then
  echo "[deploy] Missing runtime env file: $env_file" >&2
  echo "[deploy] Create it with TELEGRAM_API_ID and TELEGRAM_DRIVE_API_KEY, then rerun." >&2
  exit 3
fi

if ! docker network inspect dmcms_internal >/dev/null 2>&1; then
  echo "[deploy] Required private Docker network dmcms_internal is missing." >&2
  exit 1
fi

git -C .. fetch --prune origin
git -C .. cat-file -e "${revision}^{commit}"
git -C .. checkout --detach "$revision"

actual_revision="$(git -C .. rev-parse HEAD)"
if [[ "$actual_revision" != "$(git -C .. rev-parse "${revision}^{commit}")" ]]; then
  echo "[deploy] Checked-out revision mismatch." >&2
  exit 1
fi

docker compose --env-file "$env_file" config --quiet
docker compose --env-file "$env_file" build telegram-drive

docker volume create aivaults_telegram_drive_data >/dev/null
if ! docker run --rm --entrypoint sh \
  -v aivaults_telegram_drive_data:/data \
  aivaults/telegram-drive-headless:local \
  -c 'test -s /data/telegram.session' >/dev/null 2>&1; then
  echo "[deploy] Image built at $actual_revision, but Telegram session is not bootstrapped yet." >&2
  echo "[deploy] Run ./bootstrap-login.sh $env_file interactively on this host, then rerun deploy.sh." >&2
  exit 4
fi

docker compose --env-file "$env_file" up -d --force-recreate telegram-drive

container_id="$(docker compose --env-file "$env_file" ps -q telegram-drive)"
for attempt in {1..24}; do
  status="$(docker inspect --format '{{if .State.Health}}{{.State.Health.Status}}{{else}}none{{end}}' "$container_id")"
  if [[ "$status" == "healthy" ]]; then
    docker compose --env-file "$env_file" ps telegram-drive
    echo "[deploy] Telegram Drive headless running exact commit $actual_revision"
    exit 0
  fi
  if [[ "$status" == "unhealthy" ]]; then
    break
  fi
  sleep 5
done

echo "[deploy] Telegram Drive did not become healthy." >&2
docker compose --env-file "$env_file" ps telegram-drive >&2
docker compose --env-file "$env_file" logs --tail=120 telegram-drive >&2
exit 1
