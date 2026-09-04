# Telegram Drive Headless for DMCMS Drama

This is the minimal always-on service used by AIVaults Drama. It lives in the Telegram-Drive fork so the Telegram/MTProto boundary stays outside DMCMS Core and outside the Drama Short theme.

## Scope

The headless runtime intentionally implements only the REST contract DMCMS Drama needs:

- `GET /api/v1/health`
- authenticated `GET /api/v1/files`
- authenticated `POST /api/v1/files`
- authenticated `GET /api/v1/files/{message_id}`
- authenticated `DELETE /api/v1/files/{message_id}`
- authenticated `GET /api/v1/files/{message_id}/download` with HTTP byte ranges

It does not run Tauri, React, WebDAV, sync, supporter UI, thumbnail generation, encrypted-drive objects, transcoding, or public sharing. DMCMS already owns ingest/FFmpeg lifecycle; this service only persists and streams the final Drama rendition bytes through Telegram.

## Production topology

```text
DMCMS API + worker
    -> private Docker network
telegram-drive:8550
    -> grammers / MTProto
Telegram
```

Do not publish port `8550` on the host and do not put this service behind a public Cloudflare hostname. DMCMS should use:

```text
http://telegram-drive:8550/api/v1
```

as the per-site Drama Telegram Drive base URL.

## First-time setup

Production keeps runtime credentials outside the Git checkout. The default runbook expects:

```text
/home/vinq179/runtime/secrets/telegram-drive.env
```

with only:

```text
TELEGRAM_API_ID=<numeric Telegram application id>
TELEGRAM_DRIVE_API_KEY=<long private key shared with DMCMS Drama>
RUST_LOG=info
```

`TELEGRAM_API_HASH` is intentionally not kept in the service environment. It is needed only while bootstrapping a Telegram session.

1. Build/deploy the exact Git revision:

```bash
cd headless
./deploy.sh <git-commit>
```

On a new host the deploy script builds the image, verifies the private `dmcms_internal` Docker network and then stops safely if the Telegram session has not been bootstrapped yet.

2. Bootstrap the persistent Telegram session interactively once:

```bash
TELEGRAM_API_HASH='<telegram-api-hash>' ./bootstrap-login.sh
```

The command prompts for the Telegram phone number, login code, and 2FA password when required. The resulting SQLite MTProto session is stored in the named Docker volume `aivaults_telegram_drive_data`. The API hash, login code and 2FA password are not part of the normal runtime after this step.

3. Rerun deployment:

```bash
./deploy.sh <git-commit>
```

The service then starts on the shared private Docker network with no host `ports:` mapping. The deploy script waits for Docker health before succeeding.

For local development only, copying `.env.example` to `.env` and running `docker compose build/up` is still supported.

## DMCMS Drama settings

For the target Drama website open:

```text
Settings -> Modules -> Drama
```

Configure:

```text
Telegram Drive base URL: http://telegram-drive:8550/api/v1
Telegram Drive API key:  <same TELEGRAM_DRIVE_API_KEY>
Telegram Drive folder ID: leave blank for Saved Messages
Superseded grace period: 7
```

Save, then click **Test Telegram Drive**. DMCMS stores the URL/key as encrypted per-site module secrets. Do not copy Telegram API ID/hash, phone login, 2FA, or the MTProto session into DMCMS.

## Folder behavior

Leaving `folder_id` blank stores Drama files in Saved Messages, which is the simplest production mode and the recommended default for the initial Drama Short deployment. Numeric channel/chat IDs are supported through Telegram dialog discovery if a site later needs a dedicated destination.

## Limits

Telegram currently limits this upload path to `2,000,000,000` bytes per file. DMCMS Drama enforces the same rendition limit before calling this service.

The service stores ordinary Telegram documents, not Telegram-Drive encrypted `.tdenc` objects, because DMCMS needs efficient browser HTTP Range playback through its stable playback gateway.

## Session lifecycle

The session is durable in the Docker volume. Normal service/container restarts do not require Telegram login again. Run the `login` command again only when the session is missing/revoked or the Telegram account needs to be changed.
