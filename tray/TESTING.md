# Tray Manual Test Checklist

Manual acceptance checklist for the fusion-tray floating supervisor UI
(issues #72 MVP, #73 right-click / self-heal / LaunchAgent, plus the
single-instance guard and T2 group stop added later). Unit tests cover
the pure logic (geometry, aggregation, sorting, font subset, single
instance); this checklist is the reproducible record for the visual and
interaction layer they cannot reach. Re-run the relevant sections after
any winit/egui upgrade or geometry refactor.

Build under test: `cargo build --release --workspace` from the repo root,
binary `target/release/fusion-tray`. Daemon must be running for the
green-path checks: `fusion-sv daemon` (or via LaunchAgent).

## L1 — Floating dot

- [ ] Dot appears at right screen edge, vertically centered (default
      position: dot edge 2px from screen edge).
- [ ] Color mapping — all CORE (T0) services Healthy: **green**; any T0
      service in Starting/Unhealthy/Restarting/Draining but none
      Stopped/Failed: **yellow**; any T0 Stopped/Failed: **red**;
      daemon unreachable (or no T0 known): **gray**.
- [ ] Breathing animation (~2s cycle) plays **only** on red/yellow;
      green and gray dots are static.
- [ ] Left-click dot expands the L2 fan; clicking the visible dot again
      (without drag) collapses it — a release after a real drag must
      NOT toggle.
- [ ] Clicks on the window area outside the drawn dot pass through to
      the underlying app (no invisible click-eating rectangle).
- [ ] Drag the dot: window tracks the pointer smoothly (no oscillation
      while dragging).
- [ ] Release near a screen edge (< 20px): dot snaps flush to that edge.
- [ ] Release mid-screen: dot stays where dropped (no snap).
- [ ] Position persists across restart — quit tray, relaunch, dot
      returns to the remembered spot (`~/.fusion-sv/tray.json`); a
      corrupted or missing file falls back to the default edge position.
- [ ] Drag to a different monitor / change resolution: dot is clamped
      back inside the visible desktop, never stranded off-screen.
- [ ] Fan/list expand direction follows the nearest edge (snap right →
      fan opens left; snap left → opens right; snap top → opens down;
      snap bottom → opens up).

## L2 — Tier fan

- [ ] Click dot → fan expands in well under 200ms (perceptually
      instant; no fade-in lag).
- [ ] Fan shows 3 sectors in order: CORE (first sector from the
      opening-side edge), PLATFORM (middle), APP (last).
- [ ] Hovering a sector highlights it and shows the tier label + health
      counts (total / running).
- [ ] Clicking a sector opens the L3 list for that tier.
- [ ] Click anywhere outside the fan collapses it back to the dot.

## L3 — Tier list

- [ ] Search box filters rows live, case-insensitive, on service name
      substring; clearing restores all rows.
- [ ] Default ordering: running services (Healthy/Starting/Unhealthy)
      first, then the rest, registry order stable within each group.
- [ ] Start/stop toggle: clicking the toggle on a Stopped row flips the
      row optimistically (state changes immediately), the service
      actually starts, and the next poll confirms or corrects it; same
      in reverse for a running row.
- [ ] Pin toggle (PLATFORM and APP rows only): 📌 = pinned auto
      (boot-auto), 📍 = not pinned; toggling persists via
      `POST /api/v1/boot/pin` and survives a tray restart. CORE rows
      have no pin button.
- [ ] APP (T2) panel: the stop-all / start-all group button actually
      stops (or starts) every service in the tier — verify with
      `fusion-sv status` that nothing remains running after stop-all.
- [ ] Right-click a row opens the context menu with restart / drain /
      rollout.
- [ ] Right-click **restart** executes immediately (no confirm dialog).
- [ ] Right-click **drain** / **rollout** open the confirmation bubble
      first; cancel dismisses without side effects; confirm sends the
      command and the service reaches the expected state.

## Daemon-down & self-heal (#73)

- [ ] Stop the daemon (`fusion-sv shutdown`): dot turns **gray**, the
      panel shows the "daemon 未运行" state.
- [ ] The self-heal button appears; clicking it spawns `fusion-sv up`
      (or the bundled equivalent); once the daemon answers polls again
      the dot recovers to green and normal panels return.
- [ ] While unreachable, no crash: tray keeps running and keeps
      retrying; killing and restarting the daemon mid-panel does not
      panic the tray.

## Single-instance guard

- [ ] With one tray running, launching a second `fusion-tray` exits
      immediately with a log line (no second dot appears).
- [ ] After force-killing the tray, the stale lock socket does not
      block the next launch (recovers automatically).

## LaunchAgent auto-start (#73)

- [ ] With `~/Library/LaunchAgents/com.fusion.tray.plist` installed
      (generated from `scripts/com.fusion.tray.plist.template`), a
      fresh login brings up the tray automatically alongside the
      daemon (`com.fusion.supervisor.plist`).
- [ ] Tray connects to the auto-started daemon without manual
      intervention (dot green after boot settles).

## Regression notes for future passes

- After adding any Chinese label to `tray/src/*.rs`, run
  `bash tray/gen_font_subset.sh && cargo test -p fusion-tray` — the
  font-subset test fails the build if a glyph is missing (do not just
  add the character to the `FONT_MISSING` exception list).
- Geometry changes: the 12 pure-logic unit tests in `pos_tests`
  (clamp / snap / fan direction / window geometry / position-file
  load-save) must stay green; this checklist then re-verifies the
  visual result.
