#!/usr/bin/env bash
set -euo pipefail

cd "$(dirname "$0")"

env_file="${1:-/home/vinq179/runtime/secrets/telegram-drive.env}"
if [[ ! -f "$env_file" ]]; then
  echo "Missing runtime env file: $env_file" >&2
  exit 3
fi

if [[ -z "${TELEGRAM_API_HASH:-}" ]]; then
  echo "Set TELEGRAM_API_HASH only for this interactive command." >&2
  echo "Example: TELEGRAM_API_HASH='<hash>' ./bootstrap-login.sh '$env_file'" >&2
  exit 2
fi

docker volume create aivaults_telegram_drive_data >/dev/null
docker compose --env-file "$env_file" run --rm \
  -e TELEGRAM_API_HASH \
  telegram-drive login
