# Telegram Drive Headless for DMCMS Drama

This is the minimal always-on service used by AIVaults Drama. It lives in the Telegram-Drive fork so the Telegram/MTProto boundary stays outside DMCMS Core and outside the Drama Short theme.

## Scope

The headless runtime intentionally implements only the REST contract DMCMS Drama needs:

- `GET /api/v1/health`
- authenticated `GET /api/v1/files`
- authenticated `POST /api/v1/files` for legacy single-file compatibility
- authenticated `POST /api/v1/files/chunks` for raw bounded chunk streaming
- authenticated `GET /api/v1/files/{message_id}`
- authenticated `DELETE /api/v1/files/{message_id}`
- authenticated `GET /api/v1/files/{message_id}/download` with HTTP byte ranges
- authenticated `PUT/DELETE /api/v1/playback-assets/{asset_id}` for the durable logical-video playback registry
- origin-key protected `GET /api/v1/playback/{asset_id}/segments/{segment_index}` for fixed 64 MiB CDN origin segments

It does not run Tauri, React, WebDAV, sync, supporter UI, thumbnail generation, encrypted-drive objects, transcoding, or public sharing. New Drama uploads are not staged or transcoded by DMCMS: the browser sends bounded chunks to DMCMS, DMCMS streams each chunk to `/files/chunks`, and this service immediately stores that chunk as an ordinary Telegram document.

`POST /files/chunks` is an AIVaults headless extension, not part of the stock desktop REST API. It requires `Content-Length`, accepts at most 64 MiB per request, reads the raw body as a stream, and does not create a temporary file in the headless container.

## Production topology

Upload/control traffic remains private:

```text
DMCMS API + worker
    -> private dmcms_internal Docker network
telegram-drive:8550
    -> grammers / MTProto
Telegram
```

Visitor playback bypasses DMCMS entirely:

```text
Browser
    -> video.aivaults.top Cloudflare Worker/CDN
    -> cached 64 MiB segment HIT, or on MISS:
       telegram-origin.aivaults.top Cloudflare Tunnel
       -> private dmcms_internal network
       -> telegram-drive:8550
       -> Telegram MTProto
```

Do not publish port `8550` on the host. The origin hostname is only a Cloudflare Tunnel route to the same private container and its playback segment endpoint rejects requests that do not carry the derived origin key. DMCMS continues to use:

```text
http://telegram-drive:8550/api/v1
```

as the per-site Drama Telegram Drive control/upload base URL. The browser never receives that URL, the Telegram Drive API key, Telegram message IDs, or MTProto session material.

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

## Direct Drama upload and limits

DMCMS Drama currently splits a logical MP4 into 32 MiB storage chunks. Every chunk becomes one Telegram document and is far below Telegram's per-file limit. After verification DMCMS registers the ordered Telegram parts here as one logical playback asset. The headless service exposes that asset to Cloudflare as immutable 64 MiB origin segments; one logical CDN segment may span multiple Telegram documents. Cloudflare's Worker reconstructs the browser's logical HTTP Range response from those cached segments. DMCMS does not carry visitor video bytes.

Because the provider limit applies to each stored chunk rather than the logical MP4, a Drama video may be larger than 2 GB without creating any single Telegram file near that size. DMCMS still caps the number of chunks and validates every part checksum/size before making a new video version current.

The legacy multipart `POST /files` endpoint retains its `2,000,000,000`-byte single-file guard for compatibility with older Drama video rows and upstream-style clients.

The service stores ordinary Telegram documents, not Telegram-Drive encrypted `.tdenc` objects, because the Cloudflare delivery layer needs efficient segment reads from Telegram without downloading or transcoding a whole MP4 first.

## Session lifecycle

The session is durable in the Docker volume. Normal service/container restarts do not require Telegram login again. Run the `login` command again only when the session is missing/revoked or the Telegram account needs to be changed.
