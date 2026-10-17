# Changelog

All notable changes to FluxION will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Note:** When editing this changelog, reference the commit hash at the time of the previous edit to
track what changes have been documented. Last edit: commit `1f2c17e` (2026-02-22).

## [Unreleased]

## [0.2.61] - 2026-10-17

### Fixed
- Inverter mode changes now happen at 15-minute block boundaries. Inside a block the mode only changes for a user action (levers, VPR, kill switch, fixed slot, default mode, config change) or a safety stop; planner re-decisions wait for the next block. This ends the write at :00 being undone at :05 on every block.
- The planner no longer keeps Hold forever once applied: Self-Use/Hold hysteresis now compares keeping the mode for the whole run the plan would switch for, and the projection of later blocks uses the same rule as the current block.
- The `pricing.spot_buy_fee_czk` / `spot_sell_fee_czk` add-on options are now honoured (they were ignored and 0.5 was used).
- Dashboard labels planner Hold blocks as "Hold" instead of "Self-Use".
- The backtest page is opt-in (`FLUXION_BACKTEST_DB`); it no longer returns errors with developer file paths on the add-on.

## [0.2.60] - 2026-10-16

### Fixed
- Add-on start-up banner reports `system.debug_mode` (it always printed "Debug mode: true").
- Dashboard in the Backup default mode: the predicted SOC line stops at the Back Up minimum (the planner's floor) instead of draining to the hardware minimum; PV still charges above it, Force Charge/Discharge blocks are unchanged. Chart bars, tooltip and Upcoming Schedule show the blocks the executor runs as Back Up Mode as "Back Up Mode (25%)" in the Back Up colour. The chart refresh now updates the predicted SOC line, and the chart and schedule refresh right after the default-mode switch.
- `history.consumption_entity` and `history.solar_production_entity` are add-on options. When unset, the consumption entity is derived from the first inverter's prefix (`sensor.<prefix>_today_s_import_energy`), so sites whose prefix is not `solax` get daily summaries and the EMA.

## [0.2.59] - 2026-10-16

### Fixed
- Dashboard price chart: HDO tariff periods show as a strip when a block's grid fee is 0, and the chart's fee bars use the same pricing HDO fees the planner adds (they came from the WinterAdaptiveV3 settings).
- Avg Consumption works when there are no daily import summaries, and is drawn at the right hours (the profile is per UTC hour).
- PV history is averaged into 15-minute blocks, so the PV line fills in; new Solar Forecast line for the remaining blocks, on by default.

### Added
- `inverters[].pv_extra_prefixes`: read-only PV of parallel units (e.g. a SolaX Ultra slave) added to the inverter's PV power and solar energy for the dashboard and forecast accuracy. Nothing is ever written to these units, and their battery, grid, house-load and mode sensors are not read.

## [0.2.58] - 2026-10-16

### Added

- Default mode switch on the dashboard: Self-Use or Backup. In Backup the planner's Self-Use blocks
  run as SolaX Back Up Mode, the inverter's backup minimum SOC is set to the chosen target, and the
  planner never discharges below it. `PUT /api/user-control/default-mode`.

### Fixed

- The backup minimum SOC is re-written whenever the target changes, also while the inverter is
  already in Back Up Mode, and Back Up Mode is never entered with an unknown minimum.

## [0.2.57] - 2026-10-16

### Fixed

- The planner load comes from the 7-day house-load profile even when the consumption history
  entity is missing. Before, such a site planned on the inverter's live house-load reading, which
  on a parallel master includes the slave's charging (about 17 kW) and fed back into grid charging.
- Without load history the planner uses the configured average load, never the live reading.
- Winter Reserve re-plans as soon as the load profile or the PV forecast changes.

## [0.2.56] - 2026-10-16

### Fixed

- Winter Reserve plans and buys on the expected solar and load forecast. The conservative view
  (PV x pv_confidence, load x safety margin) now only sizes the night reserve and the export guard,
  so the planner no longer grid-charges energy the sun will deliver.
- Winter Reserve recognises the coming morning when the plan starts in the last hours of the night
  (before 08:00 local). It used to need a 4 h night inside the plan, so from about 05:15 the whole
  battery became the night reserve.

### Added

- One `WinterReserve horizon:` log line per replan and the planner input in
  `/data/winter_reserve_last_plan.json`, so a plan can be replayed offline.

## [0.2.55] - 2026-10-16

### Fixed

- **Add-on options can be saved again.** 0.2.52–0.2.54 declared
  `solar_forecast.actual_pv_power_entities` with an enum schema, so Supervisor rejected every
  options save. It is now a list of entity ids.

## [0.2.54] - 2026-10-16

### Fixed

- **Solar forecast accuracy check is more robust.**
  - If HA's entity registry is unreachable at startup, provider discovery retries every 5 minutes
    instead of staying on name-only matching.
  - A day is counted only when every roof plane's forecast and every PV sensor's statistics are
    complete.
  - Actual production is read after 22:00.
  - `sensor.fluxion_solar_forecast_bias` is republished every 15 minutes, so it survives an HA
    restart.

## [0.2.53] - 2026-10-16

### Fixed

- The add-on image builds again. 0.2.52 failed because the published workspace differs from the
  lockfile's. This version carries everything listed under 0.2.51 and 0.2.52.

## [0.2.52] - 2026-10-16

### Changed

- **Solar forecast uses Open-Meteo only.** Forecast sensors are picked by integration
  (`solar_forecast.provider`, default `open_meteo_solar_forecast`), so a second provider such as
  Forecast.Solar is no longer added on top. Several Open-Meteo entries (one per roof plane) are
  still summed.

### Added

- **Solar forecast accuracy check.** FluxION stores each morning's forecast for the day, compares
  it with the actual PV production in the evening (`solar_forecast.actual_pv_power_entities`,
  default each inverter's `pv_power_total`) and publishes `sensor.fluxion_solar_forecast_bias`.
  When the 7-day bias exceeds 10 %, it posts a notification to fix the Open-Meteo setup.

### Fixed

- The add-on image is built from `Cargo.lock`; 0.2.51 failed to build after an upstream crate
  was yanked. 0.2.51's fix (keep Self-Use in solar blocks) ships in this version.

## [0.2.51] - 2026-10-16

### Fixed

- **No more Self-Use / Hold flipping in the morning.** The 1 CZK switch threshold from 0.2.50 now
  also keeps Self-Use in blocks with a solar forecast. A held Hold is still never kept while solar
  is expected, so it can't block solar charging.

## [0.2.50] - 2026-10-16

### Fixed

- **Winter Reserve no longer flips between Self-Use and battery Hold at night.** A switch between
  the two now needs at least `mode_switch_min_benefit_czk` (default 1.0 CZK) of expected gain over
  the mode currently planned. Only the live block is affected, and only in blocks without solar.
  Grid charging and export decisions are unchanged. Set it to 0 to switch it off.

## [0.2.49] - 2026-10-15

### Fixed

- **Wake the SolaX before a grid charge, not at it.** A SolaX in Idle Mode takes about 5 minutes to
  reach Normal. FluxION now presses `battery_awaken` up to 10 minutes before a planned Force Charge,
  so the cheap block isn't lost. It never presses while automation is disabled, a VPR window is
  active, user control is off, the battery is full, or in debug mode.
- **The "grid charging not running" check waits until the inverter reports Normal mode** (up to 15
  minutes), and stays silent if the charge run has already ended.

## [0.2.48] - 2026-10-15

### Added

- **Separate buy-price sensor** (`pricing.buy_price_entity`). Use it for sites whose buy price is a
  different HA sensor from the spot/sell price, for example spot × 1.21 with VAT. A failed read
  reuses the last good buy prices; blocks the sensor doesn't cover use spot × the observed buy/spot
  ratio. The sensor is read at most once a minute. If unset, nothing changes.

### Fixed

- **The dashboard entities, lock guard and night usage now use the configured inverter's prefix**
  instead of the `solax` default.
- **No more Hold ↔ Self-Use flapping at the SOC floor overnight.** When both modes leave the battery
  and grid the same, the applied mode is kept (only in blocks without PV, so PV charging is never
  blocked).

## [0.2.47] - 2026-10-15

### Added

- **SolaX / SolaX Ultra auto-detection.** At startup FluxION checks which battery-SOC sensor each
  configured SolaX inverter has (`_battery_capacity` = SolaX, `_battery_total_capacity_charge` =
  Ultra) and uses the matching entities, whatever `vendor` says. A mismatch is logged as a warning.

### Fixed

- **HotStandBy is resolved per inverter.** In an HA with several SolaX inverters, FluxION only uses
  the configured inverter's own HotStandBy entity (`switch.<id>_hotstandby`,
  `select.<id>_hotstandby`, or the registry entry from the same integration entry). It never uses
  another inverter's. The missing-candidate probe no longer logs an error.
- The charge-running check works on SolaX Ultra, which has no `inverter_bdc_status` sensor.

## [0.2.46] - 2026-10-15

### Added

- **SolaX readiness guards before every inverter write.**
  - **Unlock:** if `select.<id>_lock_state` is "Locked", FluxION selects "Unlocked" and waits for the
    read-back (up to 10 s) before writing. "Unlocked - Advanced" is left alone. If the inverter stays
    locked, the write is skipped and retried. `control.auto_unlock_inverter` now defaults to true.
  - **HotStandBy:** found by name or in the HA entity registry (`control.hot_standby_entity`
    overrides). Checked every 5 minutes and switched back on if off (at most once an hour, with a
    notification). Set `control.ensure_hot_standby: false` to disable.
  - **Wake:** before a grid charge, if the inverter is not in Normal mode, FluxION presses
    `button.<id>_battery_awaken`. If the battery is not charging 5 minutes after a charge starts, you
    get a notification.

## [0.2.45] - 2026-10-14

### Added

- **Node-RED dashboard entities.** FluxION publishes `sensor.fluxion_pv_work_mode_plan` (same
  attributes as Node-RED's `sensor.pv_work_mode_plan`: `date`, `blocks`, `prices`, `price_stats`),
  `sensor.fluxion_aktivni_pracovni_rezim` and `sensor.fluxion_prum_nocni_vyuziti_baterie`.
  Dashboard cards only need their entity ids changed. `prices` covers today.
- **Inverter lock guard (SolaX).** When `select.<id>_lock_state` reads "Locked", FluxION creates
  one persistent notification; with `control.auto_unlock_inverter: true` (default on) it selects
  "Unlocked" once, never in debug mode.
- **Daily plan notification** at 00:15 with `notifications.daily_plan: true` (default off).
- **SolaX readiness guards.** Every inverter write first checks the lock (unlock and read-back,
  or skip the write when `control.auto_unlock_inverter` is off; default on), wakes the inverter
  with `button.<id>_battery_awaken` before ForceCharge when the run mode is not Normal (at most
  every 10 min), and warns once if the battery is not charging 5 minutes after ForceCharge.
  HotStandBy (`control.hot_standby_entity`, auto-detected, `control.ensure_hot_standby`, default
  on) is checked at startup and every 5 minutes and turned on at most once an hour.

### Fixed

- **House-load profile now actually loads.** The 7-day history request timed out after 10 s; it
  now uses a minimal response with a 60 s timeout. A fallback to the import profile logs a warning
  with the reason.
- **Profile robustness.** Hours with no data take their neighbours' average instead of 0; the 75th
  percentile is capped at 1.5× the mean so a few EV-charging days don't make the planner hold all
  day.
- A blocked Force Discharge fixed slot no longer resends Self Use every 5 minutes.
- Enabling force discharge no longer blocks saving the configuration (it is a warning).
- An export-limit restore is kept until confirmed, gives up after three rounds, and respects a
  lower limit set by the owner.
- Lever migration after an HA restart no longer skips untouched helpers.

## [0.2.44] - 2026-10-14

### Fixed

- **Evening battery planning.** The planner's load profile came from grid import, which is near zero
  at midday and low in the evening because the battery covers it, so it expected about half the real
  evening load and spent the battery before the most expensive hours. The profile now comes from the
  house-load power (`history.house_load_power_entity`, default the SolaX house load), time-weighted
  over 7 days, and the planner uses the higher of the mean and the 75th percentile times
  `control.planner_load_margin` (default 1.10). With a short battery it now holds charge in the
  mid-priced hours before the peak.
- **Lever migration** never overwrites a helper the user has changed.

### Safety

- **Export limit never exceeds the site cap.** Every export-limit write is clamped to the lowest of
  the inverter entity's maximum, `control.maximum_export_power_w` and the new optional
  `control.export_limit_hard_cap_w`. The adapter refuses a write above the entity maximum, or when
  the maximum cannot be read. A restore goes only to the exact value read back before curtailing;
  with an unknown previous limit FluxION does not curtail. Curtailment writes
  `control.export_limit_min_w` (default 0).
- **Export curtailment is off by default** (`control.export_curtailment_enabled: false`).
- **Battery force discharge is off by default** (`control.force_discharge_enabled: false`). SolaX
  Force Discharge runs at full battery power and is not yet capped to the export limit, so the
  planner does not schedule it, VPR "Vybíjení baterie" applies Self Use, and execution never sends
  it.

## [0.2.43] - 2026-10-14

### Added

- **FVE-Automatizace levers in FluxION.** FluxION creates its own HA helpers ("FluxION …") for the
  manual work mode (VPR) with start/stop, the automation kill switch, morning/evening discharge
  hours and minimum SOC, and the force-charge / no-discharge price thresholds. Values are copied
  once from the Node-RED entities (the VPR itself is not copied). Node-RED is no longer needed for
  inverter control.
- **VPR (manual work mode)** has the highest priority, also above the kill switch. It is a one-time
  window: it is applied once and reset to "Žádný" when it ends. A window across midnight survives an
  HA restart.
- **Kill switch.** With "FluxION Zakázat automatizaci" on, FluxION sets Self Use once and then
  leaves the inverter alone (VPR still applies). Turning it off re-reads the inverter's real mode.
- **Planner levers.** Price ≤ force-charge threshold → charge; price ≤ no-discharge threshold → Back
  Up (no discharge); morning/evening discharge limit export to the best N hours of each window and
  never below the set SOC. Lever changes re-plan immediately.
- **"Prodej do sítě"** maps to SolaX Feed-in Priority.
- **Export curtailment.** At sell price ≤ `control.export_curtail_at_or_below_czk` (default 0) the
  export limit is set to 0 W and the inverter's previous limit is restored afterwards.
- **Fixed buy price.** With `use_spot_prices_to_buy: false` the planner uses `fixed_buy_price_czk`
  at local time plus the HDO distribution fee.

### Fixed

- **Debug → live** switched at runtime now applies the planned mode immediately.
- **Mode drift.** If another controller changes the inverter mode, FluxION re-applies its mode (at
  most every 5 minutes; never while the kill switch is on). SolaX manual sub-modes are read exactly.
- **Chart battery forecast** uses the 7-day load profile, the PV forecast and a night window from
  yesterday's PV, so the SOC line falls through the evening and night.

### Changed

- The per-block "profit" is no longer shown; a daily saving against plain self-use follows.

## [0.2.42] - 2026-10-14

### Fixed

- **Price chart HDO times.** The dashboard matched local HDO windows against UTC block times, so
  high/low tariff colours and effective prices were drawn two hours late (CEST). Planning was not
  affected; it already priced blocks in local time.
- **Duplicate HDO sensors.** When two HDO sensors match (e.g. an old leftover set and the device's
  set), FluxION now picks the most recently updated one instead of the first by name.

## [0.2.41] - 2026-10-14

### Changed

- **HDO auto-detection.** FluxION finds the ČEZ HDO sensor by its content, not its name. It prefers
  a sensor with a `schedule` attribute (the "HDO schedule" sensor), then one with `raw_json`
  signals, then the configured name prefix. This works with site-specific suffixes (`_5946_evv1`)
  and renamed entities. The chosen entity is logged once.
- The `schedule` attribute (explicit NT/VT windows with UTC offsets) is parsed as an HDO source.

### Fixed

- The add-on no longer logs `ERROR Entity not found: sensor.cez_hdo_raw_data` on every HDO poll when
  the entity has a suffix.

## [0.2.40] - 2026-10-14

### Added

- **Winter Reserve strategy** (`strategies.winter_reserve`, enabled by default, priority 120). It
  plans the next 24–48 h every 15 minutes with dynamic programming over battery energy:
  - Charges in the cheapest blocks, from solar or grid. Grid charging only happens at or below the
    median buy price.
  - Sells only when `sell − cheap_ref / efficiency − wear ≥ min_arbitrage_profit_czk` (default 3
    CZK). `cheap_ref` is the average of the cheapest 20% of horizon buy prices.
  - Keeps a night reserve, so the battery covers the house until solar covers the load again. Night
    is detected from the PV and load forecasts, not clock hours. Reserve energy is only sold at
    `insane_price_multiplier × cheap_ref` (default 3×).
  - Re-plans when prices, SOC or the horizon change. Runtime guards stop exports near the reserve
    and stop charging near max SOC.
- Per-block solar forecasts (Open-Meteo `watts` / `wh_period`) are passed to strategies.
- Display timezone: the UI shows times in the Home Assistant timezone. When that is unknown, it
  shows them in a timezone chosen in the settings (`system.display_timezone`), with a banner
  prompting the choice. Every timestamp is converted on its own, so DST days are correct.
- History export and replay tools for backtesting strategies (`tools/winter_reserve`,
  `examples/winter_reserve_replay.rs`).

### Fixed

- The HDO tariff was never applied to scheduling, so every block used the high grid fee. HDO windows
  were also matched in UTC instead of local time.
- The operator buy fee was missing from effective buy prices when buying at spot.
- The hourly consumption profile lost the hour around the local-midnight sensor reset.
- The short force-run filter no longer overrides Winter Reserve's planned single-block actions.

### Changed

- `auto_update` (the `fluxion-upgrader` self-update mode) now defaults to `false`. The add-on starts
  FluxION directly, as in 0.2.37, until the upgrader has been field-tested.
- All internal time is UTC; local time is only used at ingest (HDO windows) and display.

## [0.2.38] - 2026-02-22

### Added

- **`fluxion-upgrader` crate** — New OTA self-updating binary that runs as PID 1 in Docker
  containers. Manages the FluxION binary lifecycle with automatic over-the-air updates via GitHub
  Releases:
  - Release detection via GitHub Releases API with configurable `release_branch` (nightly/stable)
  - SHA-256 verification of downloaded binaries for security
  - Atomic update operations with automatic rollback on failure
  - Calm-period detection before upgrades (waits for low-activity window to minimise disruption)
  - Backup/restore subsystem preserving the previous binary for rollback
  - Process supervisor managing the child `fluxion` process, forwarding signals, restarting on
    crash, and coordinating graceful shutdown during upgrades
  - Persistent state across restarts (`/data/upgrader-state.json`)
  - Upgrade event telemetry reporting
  - CLI flags: `--verbose`, `--dry-run`, `--check-update`
  - 30 unit tests covering core modules
- **Binary release CI workflow** (`.github/workflows/build-release-binaries.yml`) — Builds and
  publishes `fluxion` and `fluxion-upgrader` binaries for `x86_64` and `aarch64` on every GitHub
  release tag, using a self-hosted runner for the ARM target.
- **aarch64 cross-compilation support in Nix flake** — `flake.nix` updated to allow targeting
  `aarch64-unknown-linux-musl` for statically-linked ARM releases.

### Changed

- **CI pipeline** (`.github/workflows/ci.yml`) updated to integrate the new binary release jobs and
  self-hosted runner configuration.
- **GitLab CI** (`.gitlab-ci.yml`) extended with additional stages aligned with the multi-repo
  publish pipeline.
- **Dockerfile** updated to accommodate the upgrader-as-PID-1 model.

### Documentation

- **Major docs restructure** — All developer and operational documentation reorganised under a clear
  hierarchy:
  - `docs/architecture/` — ARCHITECTURE, ECONOMIC_OPTIMIZATION, SCHEDULING, REMOTE_ACCESS,
    BUILD_SYSTEM
  - `docs/guides/` — CONFIGURATION, DEPLOYMENT, TESTING, I18N, NIX_DOCKER, WEB_UI, CUSTOM_STRATEGIES
  - `docs/operations/` — CI_CD, DEPLOYMENT_FLOW, UPGRADER
  - `docs/reference/` — STRATEGIES, SCHEDULING_CONSTRAINTS, SIMULATION, COST_CALCULATION, TELEMETRY,
    SOLAX
  - `docs/vision/` — Premium subscription, tablet dashboard, AI automations specs
  - Stale root-level docs (DEPLOYMENT_FLOW.md, DEPLOYMENT_SETUP.md, README-Docker.md, etc.) removed
- **`docs/operations/UPGRADER.md`** — Comprehensive guide covering upgrader architecture, config
  options, deployment model, update flow, rollback behaviour, and troubleshooting.
- **Monorepo `CLAUDE.md`** — Created root-level development guide covering repository layout, Nix
  shells, common commands, CI/CD, and architecture overview.
- **`fluxion-mobile/CLAUDE.md`** — Added mobile app development guide.

## [0.2.37] - 2026-02-15

### Added

- **Telemetry pipeline** — Heartbeat client now collects full telemetry from `WebQueryResponse`
  (inverter state, schedule, prices, SOC predictions, consumption, solar, HDO) and sends it to
  `fluxion-server` every 5 minutes.
- **Schedule telemetry types** — `ScheduleBlockTelemetry`, `ScheduleTelemetry`, and
  `SocPredictionPoint` in `fluxion-shared` for tracking every strategy decision per block.
- **Server DB expanded** — New `schedule_blocks` and `soc_predictions` tables with foreign keys to
  `telemetry_snapshots`; 8 new columns on `telemetry_snapshots` for aggregated metrics.
- **Python analysis CLI** (`analysis/analyze_telemetry.py`) — 5 subcommands (summary, decisions,
  accuracy, prices, charts) for analyzing telemetry data from the server SQLite DB.
- **`fluxion-mobile-types` shared crate** — Compile-time API contract validation between server
  (`fluxion-web`) and mobile app (`fluxion-mobile`), replacing runtime JSON field parsing.
- **Parameter sweep analysis** — Script and comprehensive results CSV for strategy optimization.

### Changed

- Mobile API endpoints and credentials module refactored to use shared types from
  `fluxion-mobile-types`.
- Heartbeat handler extracts and stores schedule blocks and SOC predictions from telemetry.
- Telemetry cleanup tasks now include `schedule_blocks` and `soc_predictions` tables.

### Documentation

- **Mobile architecture documentation** — Comprehensive CLAUDE.md section covering:
  - QR-code-based pairing flow with x25519 key generation and Tor v3 client auth
  - Security model (Tor hidden service, device authentication, encrypted storage)
  - Connection architecture diagram showing full data flow from QR scan to authenticated requests
  - Mobile API endpoints (served over Tor) and server-side admin API
  - Shared API types (`fluxion-mobile-types`) with modification guide
  - Server-side file layout (`./data/tor/` structure, torrc generation, device registry)
  - Offline-first caching strategy (UI bundle versioning, state snapshots)
  - Build requirements and cross-compilation notes (Android SDK, NDK, bundled sqlite3)
- **Documentation maintenance rules** — Added to CLAUDE.md: guidelines for updating docs after
  implementing new features (Project Structure, data flows, types, usage instructions).
- **Project memory** — Created MEMORY.md with crate dependency map, key patterns, mobile API
  architecture notes, files modified per feature, and common pitfalls.

## [0.2.36] - 2026-02-15

### Added

- **Mobile remote access via Tor** — Full implementation of zero-cloud-dependency mobile monitoring
  and control over Tor hidden services:
  - Server-side Tor hidden service infrastructure with x25519 client authorization.
  - QR-code-based device pairing protocol.
  - REST API for remote status, pairing, device management, and mobile control.
  - Remote access management page in the HA dashboard.
  - Tauri 2.0 mobile app (Android) with offline-first architecture, PIN lock screen, dark theme UI,
    battery SOC gauge, energy flow display, and canvas price chart.
  - Real Arti Tor client replacing stub, with persistent credential storage.
  - Mobile UI bundle versioning and automatic update checks.
  - Android build support via `nix develop .#mobile` devShell (Android SDK 36, NDK 26, JDK 17).
  - English and Czech translations for mobile UI.
- **Winter Adaptive V10 strategy** — Dynamic battery budget allocation strategy with configurable
  overrides via TOML files in simulations.
- **Winter Adaptive V20 strategy** — Enhanced adaptive budgeting with DayMetrics-driven parameter
  resolution for market-aware behavior, configurable thresholds for solar availability, volatility,
  price levels, and tomorrow's outlook.
- **Fixed Price Arbitrage strategy** — New strategy for leveraging fixed-price energy contracts with
  spot market selling, including dynamic spot sell price computation.
- **Day profiling module** (`day_profiling.rs`) — Precise day metrics computation including price
  statistics (coefficient of variation, spread ratio, negative fraction), solar and consumption
  ratio estimations, and daily consumption forecasting.
- **NoChargeNoDischarge battery mode** — New inverter operation mode to hold battery charge while
  powering the house from grid, with full scheduling and simulation support.
- **HoldCharge action** — Preserve battery SOC using BackUpMode for upcoming expensive hours, with
  smart charge block calculation, consecutive charge grouping, gap bridging, and short self-use gap
  removal.
- **Solar forecasting enhancements** — New fields for remaining-today and tomorrow solar forecasts,
  sunrise/sunset estimation, and solar-weighted block cost estimation.
- **Heartbeat monitoring system** — `fluxion-shared` crate with centralized telemetry and heartbeat
  types; heartbeat client sends periodic status updates to `fluxion-server`.
- **`fluxion-server` binary** — Standalone monitoring server with dashboard, database, email alerts,
  and NixOS module, deployed via automated scripts.
- **`fluxion-version` binary** — Simple binary to print the package version.
- **Ralph Loop optimization framework** — Iterative AI-driven strategy optimization with benchmark
  tools, C10 parameter sweep analysis scripts and charts, and tracking prompts.
- **Strategy configuration overrides** — Support for `--strategy-config <path.toml>` in simulations
  for runtime parameter tuning.
- **Strategy registry** — Centralized registry for all strategies (V1-V10, V20, C10, C20) with
  listing, info, and dynamic configuration support.

### Changed

- **Profit calculation** improved to use effective price (spot + grid fee + buy fee) for import
  costs and subtract battery average charge cost basis from savings.
- **Default thresholds adjusted** — `min_export_spread_czk` lowered from 5.0 to 3.0 CZK,
  `min_soc_after_export` reduced from 35% to 25%, `expensive_level_threshold` decreased from 0.5 to
  0.3.
- **HDO entity resolution** enhanced with fallback prefix search for robustness.
- **CI pipeline** migrated to NixOS-based Docker runner image with simplified job configuration.
- **Dashboard UI** updated with solar-weighted cost estimation and savings visualization.
- **Simulation engine** updated with effective price calculation and improved block handling with
  hourly consumption profiles.

### Removed

- **Winter Adaptive V6 strategy** — Removed along with all associated configuration, modules, and
  references.
- **Deprecated logic** — Removed obsolete battery arbitrage fields, evaluation timeout from plugin
  manager, and deprecated economic calculation utilities.
- **Rust plugin adapter** — Removed `builtin/rust_adapter.rs` and unused plugin manager APIs.

### Fixed

- Clippy warnings resolved across remote access and config modules (derive Default, must_use
  annotations, idiomatic error handling, let-else patterns).

## [0.2.35] - 2026-01-27

### Fixed

- Fixed Docker build failure caused by cross-device link error (OS error 18) when
  `rust-toolchain.toml` triggered a rustup toolchain update inside Docker's overlay filesystem. The
  file is no longer copied into the Docker build context, as the stable toolchain installed by
  rustup is sufficient for release builds.

### Changed

- Reformatted documentation and scripts for consistency: `CLAUDE.md` table/paragraph line wrapping,
  `config.example.toml` alignment, and `week_simulation.sh` indentation.

## [0.2.34] - 2026-01-26

### Added

- **Winter Adaptive V8 strategy** — Top-N peak discharge optimizer targeting the absolute
  highest-priced blocks. Features predictive battery SOC simulation, configurable discharge block
  count (default 8 = 2 hours), 3 CZK minimum spread requirement, and smart export policy with SOC
  threshold protection.
- **Winter Adaptive V9 strategy** (now the default) — Solar-aware morning peak optimizer. On sunny
  days (>5 kWh solar forecast), performs minimal grid charging to cover only the morning peak (6-9
  AM) and lets solar charge the battery during the day. On low-solar days, falls back to full
  arbitrage mode like V8. Achieves ~40% savings on winter days with optimal solar utilization on
  sunny days.
- **Solar forecast integration** — Fetches solar production forecasts from Home Assistant sensors.
  Supports multiple PV arrays via sensor pattern matching. Passes today's total, remaining, and
  tomorrow's forecast through `EvaluationContext` so strategies can make solar-aware decisions.
- **User control persistence** — Manual override of battery charging/discharging with automatic
  timeout and state restoration. Includes `UserControlState`/`UserControlMode` types, file-based
  persistence, and REST API endpoints.
- **Web UI solar forecast display** — Dashboard now shows today's total solar forecast, remaining
  forecast, and tomorrow's forecast.
- **Week simulation script** (`scripts/week_simulation.sh`) — Multi-day simulation tool with SOC
  carry-over between days, cost aggregation, and savings comparison vs no-battery baseline.
- **Simulator solar forecast support** — Added `--solar none|moderate|high` CLI flag, synthetic
  solar profiles, and historical solar data loading from the database.

### Changed

- **V9 is now the default strategy** for new installations, replacing V7. Updated defaults in
  `fluxion-types`, HA addon `config.yaml`, and `config.example.toml`.
- **Existing strategies improved with solar awareness** — V7 now reduces grid charging when solar
  production is expected; V8 improved discharge timing and spread calculations.
- **EvaluationContext expanded** with `solar_forecast_total_today_kwh`,
  `solar_forecast_remaining_today_kwh`, `solar_forecast_tomorrow_kwh`, and
  `battery_avg_charge_price_czk_per_kwh` fields.

### Fixed

- **V8 config exposure** — `WinterAdaptiveV8Config` was missing from `StrategiesConfig`; V8 now
  properly receives user configuration instead of falling back to defaults.
- **Solar forecast enabled logic** — Was always evaluating to `true` regardless of config; now
  properly respects the configured default.

## [0.2.33] - 2026-01-19

### Added

- **Winter Adaptive V5, V6, and V7 strategies** — Three new arbitrage strategies with progressively
  better optimization. V7 is the new default strategy, featuring unconstrained multi-cycle arbitrage
  that achieves 22-87% cost reduction across all market scenarios (volatile, negative prices, usual
  day, elevated, HDO).
- **Winter Adaptive V4 strategy** — New strategy variant with full configuration pipeline and Home
  Assistant addon support.
- **Strategy Simulator web UI** — New "Simulation" tab in the web dashboard allowing interactive
  strategy comparison against synthetic price scenarios.
- **CLI strategy simulator (`fluxion-sim`)** — New command-line tool for running strategy
  simulations against historical SQLite data or synthetic scenarios, with CSV output support.
- **Synthetic price scenarios** — Five built-in market scenarios for strategy testing: `usual_day`,
  `volatile`, `elevated_day`, `negative_prices`, `hdo_optimized`.
- **Shared strategy modules** — Centralized pricing (`pricing.rs`), locking (`locking.rs`), and
  seasonal detection (`seasonal.rs`) modules extracted from strategies.
- **Strategy performance analysis documentation** — New docs covering strategy comparison results
  and cost calculation refactoring.
- **CLI simulator reference documentation** — Comprehensive guide for the `fluxion-sim` tool.

### Changed

- **Default strategy changed from V5 to V7** — V7 provides significantly better arbitrage
  performance: 62% better on volatile days, 87% better with negative prices, 48% better on usual
  days.
- **V3 strategy optimized** — Refactored V3 implementation with improved scheduling logic and HA
  plugin integration.
- **Web dashboard enhanced** — Added interactive schedule chart and dynamic route support.

### Fixed

- Clippy warnings resolved including range contains patterns, nested if statements, string
  conversion methods, missing `must_use` attributes, and Debug implementation improvements.

## [0.2.0] - 2025-11-30

### Code Quality & Architecture Improvements

This release focuses on code quality improvements based on comprehensive code review, eliminating
all Clippy warnings and simplifying the architecture.

### Changed

- **Fixed Clippy Warnings**: Refactored functions with 8+ parameters to use Bevy `SystemParam`
  structs

  - `poll_price_channel` now uses `PriceChannelParams` for cleaner function signatures
  - `config_event_handler` now uses `ConfigEventParams` for better maintainability
  - Result: Zero Clippy warnings across entire codebase

- **Improved Type Safety**: Replaced macro-based abstractions with typed generic functions

  - Converted `read_optional!` macro to `read_optional_sensor<F>()` async function
  - Better IDE support with full type inference
  - Enhanced debuggability and compile-time safety

- **Unified Type Definitions**: Consolidated duplicate component definitions

  - Removed duplicate components from `fluxion-core`
  - All components now defined in `fluxion-types` as single source of truth
  - Used re-exports to maintain API compatibility

- **Simplified Architecture**: Removed unnecessary abstraction layers

  - Eliminated `InverterModel` enum (redundant wrapper around `InverterType`)
  - Changed `Inverter.model` → `Inverter.inverter_type` for directness
  - Cleaner, more maintainable codebase with fewer indirection layers

### Technical Details

- **Code Quality Grade**: Improved from B+ to A- based on professional Rust review
- **Clippy Warnings**: 2 → 0 (100% clean)
- **Idiomatic Rust**: Enhanced use of Bevy ECS patterns and generic functions
- **Documentation**: Added comprehensive CODE_REVIEW.md with findings and improvements

### Documentation

- Added `CODE_REVIEW.md` with detailed analysis and implemented improvements
- Updated `ARCHITECTURE.md` to version 1.2 with code quality changelog
- Enhanced README.md with code quality badges and updated version info

## [0.1.0] - 2025-10-29

### MVP Release - Production Ready

FluxION v0.1.0 represents the Minimum Viable Product (MVP) release, ready for production use with
Home Assistant integration.

### Added

#### Core Features

- **ECS Architecture**: Built on Bevy Entity Component System for clean separation of data and logic
- **Vendor-Agnostic Design**: Generic sensor abstraction layer supporting multiple inverter brands
- **Home Assistant Integration**: Primary communication through HA API with existing vendor
  integrations
- **Economic Optimization**: Multiple strategy system for intelligent charge/discharge scheduling
  - Time-aware charging (buy during cheapest hours)
  - Winter peak discharge strategy
  - Seasonal adaptation (summer vs winter optimization)
- **Spot Price Automation**: Intelligent scheduling based on electricity spot prices
- **Battery SOC Prediction**: Accurate state-of-charge forecasting for optimization decisions
- **Mode Change Debouncing**: Prevents rapid mode switching and inverter EEPROM wear

#### Internationalization

- **Multi-language Support**: English (default) and Czech translations
- **134+ Translation Keys**: Complete coverage of UI elements
- **Fluent Framework**: Mozilla Fluent for professional i18n
- **Embedded Translations**: No external files needed

#### Web Interface

- **Real-time Dashboard**: System status and metrics monitoring
- **Interactive Charts**: Power flows visualization with Chart.js
- **Schedule Visualization**: Visual timeline of planned battery operations
- **Manual Mode Control**: Override automatic scheduling when needed
- **Multi-language UI**: Support for English and Czech
- **Dark Mode**: (future enhancement planned)

#### Monitoring & Observability

- **30+ Sensor Types**: Comprehensive monitoring including:
  - Extended PV strings (PV1-4 individual powers)
  - Three-phase measurements (L1/L2/L3 voltage, current, power)
  - Battery extended metrics (SOH, BMS limits, energy totals)
  - Grid totals (import/export, today/total yield)
  - Temperature sensors (inverter, battery, board, boost)
  - EPS status (Emergency Power Supply)
  - Fault/diagnostic data
- **Battery History**: 48-hour SOC and power history tracking
- **PV Generation History**: Solar production monitoring
- **Prometheus Metrics**: Ready for integration with monitoring tools

#### Hardware Support

- **Solax X3-Hybrid G4**: Full support via Home Assistant integration
- **Modbus TCP**: Optional feature for direct inverter communication
- **Multi-inverter Topology**: Master/slave configuration support

#### Configuration

- **Flexible Configuration**: Support for TOML, JSON, and HA addon options
- **HA Addon Options**: Native Home Assistant addon configuration
- **Battery Economics**: Configurable wear cost and efficiency parameters
- **Price Thresholds**: Customizable buy/sell price limits
- **Mode Change Intervals**: Configurable debounce to prevent rapid switching

### Changed

#### Performance & Reliability

- **Channel-Based Async**: Non-blocking async operations using crossbeam channels
- **Tokio Runtime**: Efficient async I/O for HA communication
- **15-Minute Blocks**: Aligned with most spot price granularity
- **Graceful Error Handling**: Robust error recovery and logging

#### Code Quality

- **Zero Clippy Warnings**: Clean, idiomatic Rust code
- **Comprehensive Tests**: 23 test modules covering core functionality
- **Type Safety**: Strong typing throughout the codebase
- **Documentation**: Extensive inline documentation and guides

### Fixed

- **Fragmented Charging**: Fixed issue with single-block charge operations
- **Battery Prediction**: Improved SOC prediction accuracy
- **Schedule Regeneration**: Proper handling of day-ahead price arrivals
- **Mode Switch Reduction**: Eliminated unnecessary mode changes on low SOC
- **Price Interval Handling**: Correct handling of 15-minute spot price blocks

### Technical Details

#### Architecture

- **Bevy ECS**: v0.15.0-rc.2
- **Axum Web Framework**: v0.8
- **Tokio Runtime**: Async runtime for I/O operations
- **Askama Templates**: Type-safe HTML templating
- **Fluent i18n**: Mozilla Fluent for translations

#### Deployment Options

- **Home Assistant Addon**: Native integration (planned)
- **Docker**: Containerized deployment (planned)
- **Standalone Binary**: Direct execution on Linux systems

#### License

- **AGPL v3.0**: GNU Affero General Public License
- **Commercial Licensing**: Available via info@solare.cz

### Known Limitations

- **Single Inverter Brand**: Currently only Solax fully tested (architecture supports others)
- **Forecast Integration**: Solar and consumption forecasts not yet integrated
- **Initial Battery History**: HA history fetch on startup not yet implemented
- **Work Mode Mapping**: Generic work mode mapping needs enhancement

### Documentation

- [Architecture Overview](docs/architecture/ARCHITECTURE.md)
- [Configuration Guide](docs/guides/CONFIGURATION.md)
- [Deployment Guide](docs/guides/DEPLOYMENT.md)
- [Testing Guide](docs/guides/TESTING.md)
- [Internationalization](docs/guides/I18N.md)
- [Web UI Guide](docs/guides/WEB_UI_GUIDE.md)

### Contributors

- SOLARE S.R.O. - Initial development and architecture

## [0.0.1] - 2025-01-15

### Initial Development

- Project initialization
- Basic Modbus communication
- Proof of concept scheduler

______________________________________________________________________

## Version History Summary

- **v0.2.38** (2026-02-22) - Upgrader crate (OTA, PID 1, rollback), binary release CI, docs
  restructure
- **v0.2.37** (2026-02-15) - Telemetry pipeline, schedule telemetry types, fluxion-mobile-types
- **v0.2.36** (2026-02-15) - Mobile remote access via Tor, V10/V20/FPA strategies, heartbeat
  monitoring
- **v0.2.35** (2026-01-27) - Docker build fix, formatting cleanup
- **v0.2.34** (2026-01-26) - V8/V9 strategies, solar forecast, user control persistence
- **v0.2.33** (2026-01-19) - V4-V7 strategies, strategy simulator (web + CLI)
- **v0.2.0** (2025-11-30) - Code quality and architecture improvements
- **v0.1.0** (2025-10-29) - MVP Release - Production ready
- **v0.0.1** (2025-01-15) - Initial development

## Upgrade Notes

### From Development/Alpha to v0.1.0

1. **Configuration Format**: Ensure your config uses the new TOML format with all required fields
2. **Battery Parameters**: Add battery capacity, wear cost, and efficiency settings
3. **Mode Change Interval**: Set appropriate `min_mode_change_interval_secs` (recommended: 300s)
4. **Check Logs**: Review logs for deprecation warnings

## Future Roadmap

### Planned for v0.2.0

- [ ] Additional inverter brand support (Fronius, SMA, Huawei)
- [ ] Solar forecast integration (Solcast/Forecast.Solar)
- [ ] Consumption forecast integration
- [ ] Enhanced battery history with HA initial fetch
- [ ] Improved work mode detection and mapping

### Planned for v0.3.0

- [ ] Advanced forecasting with machine learning
- [ ] Multi-battery support
- [ ] Dynamic pricing strategy optimization
- [ ] Mobile-responsive Web UI enhancements

### Long-term Vision

- Grid services participation (demand response)
- Vehicle-to-grid (V2G) integration
- Community solar sharing features
- Advanced analytics and reporting

______________________________________________________________________

For commercial licensing or support inquiries: info@solare.cz

**License:** GNU Affero General Public License v3.0 (AGPL-3.0)
