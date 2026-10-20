# Repository Agent Instructions

## $5 lifetime supporter license is a protected compatibility contract

Before changing payments, supporter entitlements, advertisements, release configuration, secure credential storage, or the supporter Cloudflare Worker, read [SUPPORTER_LICENSE_INVARIANTS.md](SUPPORTER_LICENSE_INVARIANTS.md) and [SUPPORTER_SERVICE.md](SUPPORTER_SERVICE.md).

Do not weaken or silently change the established supporter promise:

- one verified payment of exactly $5.00 USD grants the lifetime ad-free supporter entitlement;
- it is not a subscription, and existing purchasers must never be prompted or required to pay again;
- active and offline-grace entitlements must suppress all sponsor advertisements;
- normal application updates must preserve activation;
- recovery-code restoration and the supported device allowance must remain available;
- every application feature remains available to non-paying users;
- backup health must never be used as an entitlement, checkout, refresh, or ad-removal dependency.

Pricing, entitlement rights, device limits, signing-key strategy, stable credential identifiers, token compatibility, revocation policy, or recovery behavior may change only with the repository owner's explicit approval and a reviewed migration plan for existing purchasers. Never rotate the supporter signing key or stable secure-storage identifiers as an incidental refactor.

Any in-scope change must preserve the automated checks listed in `SUPPORTER_LICENSE_INVARIANTS.md`. A live PayPal transaction, production deployment, signing-key rotation, entitlement revocation, or production D1 mutation requires explicit authorization; do not infer it from a general implementation request.

## FAST DEV — Telegram headless versus native app (2026-10-20)

- Every task agent uses a clean isolated worktree from latest `origin/main`, tests/reviews and commits/pushes only completed feature changes. Do not modify another agent's session or production Telegram session files.
- **`Merge Telegram Drive`** means selected feature refs are integrated on clean latest `origin/main` **and the affected headless service is updated and smoke-tested Live in the same operation**, with exact SHA, running OCI digest, persisted Telegram session, DMCMS delivery API and no dropped/inflight upload work. Golden CI/CD must not be silently used for routine Merge.
- **Fast headless adapter not yet qualified:** fail before pushing main until its videv image build, viprod read-only candidate, safe singleton handover and rollback are verified. Long-running Telegram account sessions must not be copied into CI/build or duplicated in parallel consumers. See `/home/vinq179/aivaults-cloudflare-ops/docs/fast-dev-fleet.md`.
- **Desktop/Android binary distribution remains separate** from the hosted headless service: `Merge` does not implicitly rebuild/sign/publish installer binaries or change the $5 lifetime supporter/entitlement compatibility contract. Signed native releases require their own explicit release action.
- Keep Golden/CI release instructions below as an explicit future `Deploy chuẩn` / stable release option.

## AIVaults Golden Release Flow (explicit stable release only)

The repository has two release profiles and both start from the same canonical `origin/main` revision:

- **Desktop/Android binaries:** GitHub Actions builds/signs the exact release commit/tag and publishes immutable signed artifacts through GitHub Releases. Do not create a production release from an untracked local binary. Existing signing/entitlement approval rules above remain authoritative.
- **AIVaults headless service:** GitHub Actions/CI must build the headless container exactly once, push it to the configured registry (GHCR by default), and pin it by immutable digest. Staging/DMCMS integration validates that digest first; production pulls the same digest and does not build source on the host.

Shared rules:

- Task agents use isolated clean worktrees/branches; one release owner integrates only pushed refs in a clean Integration/Release worktree from latest `origin/main` and reruns final gates.
- Fetch main again before publish; if it moved, re-integrate/reverify and never force-push.
- Production/headless runtime does not use blind `git pull`, SCP/rsync, manual source edits, or build-on-prod as the normal release path.
- Runtime Telegram session, API credentials, signing keys, release state, and backups remain outside Git; drift fails closed.
- Headless DB/state changes, if any, require backup + one-shot compatible migration before candidate cutover.
- Long-running headless rollout should be Blue/Green or equivalent health-gated switch with previous digest retained for rollback.
- The current `headless/deploy.sh` build-on-host behavior is transitional. It may be used only with explicit temporary legacy authorization until CI image publishing/promotion is implemented; it is not “deploy chuẩn”.
- `deploy` or `cp deploy` still requires the explicit production authorization boundary stated above.
- Release recap must name exact Git SHA/tag, signed artifact checksum or image digest, validation result, production result, and rollback state.\n