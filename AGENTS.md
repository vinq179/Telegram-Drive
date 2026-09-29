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

## Canonical Git / Release Flow

- Mỗi task/agent dùng clean worktree + branch riêng từ latest `origin/main`; test, commit đúng intended files và push feature ref.
- Release owner merge only committed/pushed refs trong clean Integration/Release worktree từ latest `origin/main`, chạy final gates trên merged tree, fetch main lại trước push và không force-push.
- `origin/main` là release source of truth duy nhất. Headless deploy chỉ nhận full 40-character SHA đúng bằng current `origin/main`, dùng lock, từ chối dirty runtime tree và checkout detached exact SHA.
- Không SCP/copy source thủ công hoặc stash/reset/clean production drift để ép deploy.
- `deploy` = deploy intended current canonical release theo exact-SHA flow; `cp deploy` = commit + push intended changes -> integrate/verify -> push `origin/main` -> deploy exact resulting SHA.
- Quy tắc này **không thay thế approval boundary ở trên**: live PayPal action, production deployment, signing-key rotation, entitlement revocation và production D1 mutation vẫn cần user explicit authorization trong task hiện tại. Một yêu cầu trực tiếp như “deploy đi” hoặc “cp deploy” là authorization cho deploy đó; không được suy diễn deployment từ một yêu cầu implementation chung.
