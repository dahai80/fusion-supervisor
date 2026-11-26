---
doc_id: fusion-supervisor/changelog
product: fusion-supervisor
class: core
entry: service
doc_type: changelog
title: { zh: "fusion-supervisor 变更日志", en: "fusion-supervisor changelog" }
audience: engineer
status: draft
version: 0.1.46
updated: 2026-11-07
owner: "@dahai"
source_of_truth:
  - Cargo.toml
---

# Changelog

Format follows Keep a Changelog; entries cite the issue or commit that
carried the change. Version in `Cargo.toml` is the single source of truth
for the current version.

## Unreleased

### Added
- #168: size-based rotation for daemon logs (`log_rotate_max_mb`/`log_rotate_keep`/
  `log_rotate_paths`) — copy-truncate of the launchd StandardOutPath files every
  tick, keeping N `.N` snapshots (default 100MB / 3 backups).

### Changed
- #108: safe tray reload path — codesign + bootout/bootstrap after upgrade.
- #107: boot state reconciliation (surviving processes no longer misreported
  as Stopped) + sleep/wake diagnostics (time-jump detection).
- #106: APP-tier one-key stop now covers T1+T2 via the `app` boot group.
- #105: inject `FUSION_COWORK_WORKSPACE_ROOT` into fusion-cowork-web.
- #103: unset `FUSION_BACKUP_TARGET` degrades to a one-time WARN (optional
  feature, not a fault); explicit `backup` still fails closed.
- #101-#104: core-profile boot contract — degrade, never spam.
- #100: cowork-web daemon spec sources the root `.env` for the webhook secret.

### Added
- #99: web console Ops page — backup trigger, boot groups, boot-pin editor.

### Fixed
- #155: fusion-site probe targets the module-plane health route
  `/shell/fusion-site/api/experience/health` — the module-mode build
  (basePath `/shell/fusion-site`, upstream fusion-site#151) intentionally
  404s `/`, so the healthy process was stuck `Starting` forever.
- #153: probe Timeout no longer rides the 3-fail restart streak — timeouts
  need 15 consecutive failures (TIMEOUT_RESTART_STREAK) before a restart is
  scheduled, so a healthy fusion-mlx saturating the GIL during a 27B decode
  is not killed mid-generation; ConnectionRefused/ProcessExit keep the fast
  3-streak (real death signals), and the start-retry path keeps the fast
  threshold (a not-yet-listening process is expected).

## v0.1.39 and earlier

- #96: restart/rollout must actually replace the process PID (stop +
  stop-and-verify + start; prewarm loop no longer burns its timeout budget
  instantly).
- #95: web console bilingual i18n + shell theme alignment with UI spec.
- #94: adopt ai-booking-agent (business/ mapping + Daemon takeover).
- #93: embedded console API calls carry the shell prefix.
- #91/#92: boot tier promotion T2→T1 for menu-resident backends; shell menu
  manifest alignment.
- #90: dedicated start-retry curve 5/15/60/300s for failed starts instead of
  terminal Failed (boot dependency races self-heal).
- #89: fusion-design probe `GET /api/health` :11493; restart falls back to
  stop+start.
- Full history: `git log` in this repository; issue-level detail in
  `README.md` section "How it works".
