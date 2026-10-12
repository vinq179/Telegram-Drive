# Telegram Drive (AIVaults fork) — CI migration boundary

This fork contains **three different products**: signed desktop/Android binaries, the headless Telegram Drive service used by DMCMS, and the supporter Cloudflare Worker with independent backups. They cannot be made safe by copying a single container deployment flow.

`.woodpecker/quality.yml` is **source-only** and checks supporter compatibility plus selected client tests; the invariant contract in `SUPPORTER_LICENSE_INVARIANTS.md` stays authoritative. The headless Rust build needs a separately qualified deterministic lockfile/CI runner and container release; desktop signing needs appropriate macOS/Windows/Android toolchains.

Multi-agent integration must use isolated task worktrees and merge only pushed selected refs from latest `origin/main`, followed by merged-tree tests. Production for headless runs only after rootless videv build -> authenticated LAN Registry digest -> immutable manifest -> isolated staging with Telegram session secrets held outside CI -> version-compatible DMCMS worker coordination -> explicit approval -> reversible VIP1/VIP2 health smoke.

**DO NOT DELETE** GitHub Actions for signed platforms, supporter D1 backup, or the Cloudflare Worker until equivalent signed/artifact and disaster-recovery jobs have passed. Never rotate license keys or change the $5 lifetime supporter promise as part of CI migration.

Status: CI quality prepared locally only; no Woodpecker-to-production qualification.
