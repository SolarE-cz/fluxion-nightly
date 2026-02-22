# Changelog

All notable changes to FluxION will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

**Note:** When editing this changelog, reference the commit hash at the time of the previous edit
to track what changes have been documented. Last edit: commit `70f3f1d` (2026-02-15).

## [Unreleased]

## [0.2.37] - 2026-02-15

### Added
- **Telemetry pipeline** — Heartbeat client now collects full telemetry from `WebQueryResponse`
  (inverter state, schedule, prices, SOC predictions, consumption, solar, HDO) and sends it to
  `fluxion-server` every 5 minutes.
- **Schedule telemetry types** — `ScheduleBlockTelemetry`, `ScheduleTelemetry`, and
  `SocPredictionPoint` in `fluxion-shared` for tracking every strategy decision per block.
- **Server DB expanded** — New `schedule_blocks` and `soc_predictions` tables with foreign keys
  to `telemetry_snapshots`; 8 new columns on `telemetry_snapshots` for aggregated metrics.
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
- **Mobile remote access via Tor** — Full implementation of zero-cloud-dependency mobile
  monitoring and control over Tor hidden services:
  - Server-side Tor hidden service infrastructure with x25519 client authorization.
  - QR-code-based device pairing protocol.
  - REST API for remote status, pairing, device management, and mobile control.
  - Remote access management page in the HA dashboard.
  - Tauri 2.0 mobile app (Android) with offline-first architecture, PIN lock screen,
    dark theme UI, battery SOC gauge, energy flow display, and canvas price chart.
  - Real Arti Tor client replacing stub, with persistent credential storage.
  - Mobile UI bundle versioning and automatic update checks.
  - Android build support via `nix develop .#mobile` devShell (Android SDK 36, NDK 26, JDK 17).
  - English and Czech translations for mobile UI.
- **Winter Adaptive V10 strategy** — Dynamic battery budget allocation strategy with
  configurable overrides via TOML files in simulations.
- **Winter Adaptive V20 strategy** — Enhanced adaptive budgeting with DayMetrics-driven
  parameter resolution for market-aware behavior, configurable thresholds for solar
  availability, volatility, price levels, and tomorrow's outlook.
- **Fixed Price Arbitrage strategy** — New strategy for leveraging fixed-price energy contracts
  with spot market selling, including dynamic spot sell price computation.
- **Day profiling module** (`day_profiling.rs`) — Precise day metrics computation including
  price statistics (coefficient of variation, spread ratio, negative fraction), solar and
  consumption ratio estimations, and daily consumption forecasting.
- **NoChargeNoDischarge battery mode** — New inverter operation mode to hold battery charge
  while powering the house from grid, with full scheduling and simulation support.
- **HoldCharge action** — Preserve battery SOC using BackUpMode for upcoming expensive hours,
  with smart charge block calculation, consecutive charge grouping, gap bridging, and short
  self-use gap removal.
- **Solar forecasting enhancements** — New fields for remaining-today and tomorrow solar
  forecasts, sunrise/sunset estimation, and solar-weighted block cost estimation.
- **Heartbeat monitoring system** — `fluxion-shared` crate with centralized telemetry and
  heartbeat types; heartbeat client sends periodic status updates to `fluxion-server`.
- **`fluxion-server` binary** — Standalone monitoring server with dashboard, database, email
  alerts, and NixOS module, deployed via automated scripts.
- **`fluxion-version` binary** — Simple binary to print the package version.
- **Ralph Loop optimization framework** — Iterative AI-driven strategy optimization with
  benchmark tools, C10 parameter sweep analysis scripts and charts, and tracking prompts.
- **Strategy configuration overrides** — Support for `--strategy-config <path.toml>` in
  simulations for runtime parameter tuning.
- **Strategy registry** — Centralized registry for all strategies (V1-V10, V20, C10, C20)
  with listing, info, and dynamic configuration support.

### Changed
- **Profit calculation** improved to use effective price (spot + grid fee + buy fee) for
  import costs and subtract battery average charge cost basis from savings.
- **Default thresholds adjusted** — `min_export_spread_czk` lowered from 5.0 to 3.0 CZK,
  `min_soc_after_export` reduced from 35% to 25%, `expensive_level_threshold` decreased
  from 0.5 to 0.3.
- **HDO entity resolution** enhanced with fallback prefix search for robustness.
- **CI pipeline** migrated to NixOS-based Docker runner image with simplified job configuration.
- **Dashboard UI** updated with solar-weighted cost estimation and savings visualization.
- **Simulation engine** updated with effective price calculation and improved block handling
  with hourly consumption profiles.

### Removed
- **Winter Adaptive V6 strategy** — Removed along with all associated configuration,
  modules, and references.
- **Deprecated logic** — Removed obsolete battery arbitrage fields, evaluation timeout from
  plugin manager, and deprecated economic calculation utilities.
- **Rust plugin adapter** — Removed `builtin/rust_adapter.rs` and unused plugin manager APIs.

### Fixed
- Clippy warnings resolved across remote access and config modules (derive Default, must_use
  annotations, idiomatic error handling, let-else patterns).

## [0.2.35] - 2026-01-27

### Fixed
- Fixed Docker build failure caused by cross-device link error (OS error 18) when
  `rust-toolchain.toml` triggered a rustup toolchain update inside Docker's overlay filesystem.
  The file is no longer copied into the Docker build context, as the stable toolchain installed
  by rustup is sufficient for release builds.

### Changed
- Reformatted documentation and scripts for consistency: `CLAUDE.md` table/paragraph line
  wrapping, `config.example.toml` alignment, and `week_simulation.sh` indentation.

## [0.2.34] - 2026-01-26

### Added
- **Winter Adaptive V8 strategy** — Top-N peak discharge optimizer targeting the absolute
  highest-priced blocks. Features predictive battery SOC simulation, configurable discharge
  block count (default 8 = 2 hours), 3 CZK minimum spread requirement, and smart export
  policy with SOC threshold protection.
- **Winter Adaptive V9 strategy** (now the default) — Solar-aware morning peak optimizer.
  On sunny days (>5 kWh solar forecast), performs minimal grid charging to cover only the
  morning peak (6-9 AM) and lets solar charge the battery during the day. On low-solar days,
  falls back to full arbitrage mode like V8. Achieves ~40% savings on winter days with optimal
  solar utilization on sunny days.
- **Solar forecast integration** — Fetches solar production forecasts from Home Assistant
  sensors. Supports multiple PV arrays via sensor pattern matching. Passes today's total,
  remaining, and tomorrow's forecast through `EvaluationContext` so strategies can make
  solar-aware decisions.
- **User control persistence** — Manual override of battery charging/discharging with
  automatic timeout and state restoration. Includes `UserControlState`/`UserControlMode`
  types, file-based persistence, and REST API endpoints.
- **Web UI solar forecast display** — Dashboard now shows today's total solar forecast,
  remaining forecast, and tomorrow's forecast.
- **Week simulation script** (`scripts/week_simulation.sh`) — Multi-day simulation tool with
  SOC carry-over between days, cost aggregation, and savings comparison vs no-battery baseline.
- **Simulator solar forecast support** — Added `--solar none|moderate|high` CLI flag, synthetic
  solar profiles, and historical solar data loading from the database.

### Changed
- **V9 is now the default strategy** for new installations, replacing V7. Updated defaults in
  `fluxion-types`, HA addon `config.yaml`, and `config.example.toml`.
- **Existing strategies improved with solar awareness** — V7 now reduces grid charging when
  solar production is expected; V8 improved discharge timing and spread calculations.
- **EvaluationContext expanded** with `solar_forecast_total_today_kwh`,
  `solar_forecast_remaining_today_kwh`, `solar_forecast_tomorrow_kwh`, and
  `battery_avg_charge_price_czk_per_kwh` fields.

### Fixed
- **V8 config exposure** — `WinterAdaptiveV8Config` was missing from `StrategiesConfig`;
  V8 now properly receives user configuration instead of falling back to defaults.
- **Solar forecast enabled logic** — Was always evaluating to `true` regardless of config;
  now properly respects the configured default.

## [0.2.33] - 2026-01-19

### Added
- **Winter Adaptive V5, V6, and V7 strategies** — Three new arbitrage strategies with
  progressively better optimization. V7 is the new default strategy, featuring unconstrained
  multi-cycle arbitrage that achieves 22-87% cost reduction across all market scenarios
  (volatile, negative prices, usual day, elevated, HDO).
- **Winter Adaptive V4 strategy** — New strategy variant with full configuration pipeline
  and Home Assistant addon support.
- **Strategy Simulator web UI** — New "Simulation" tab in the web dashboard allowing
  interactive strategy comparison against synthetic price scenarios.
- **CLI strategy simulator (`fluxion-sim`)** — New command-line tool for running strategy
  simulations against historical SQLite data or synthetic scenarios, with CSV output support.
- **Synthetic price scenarios** — Five built-in market scenarios for strategy testing:
  `usual_day`, `volatile`, `elevated_day`, `negative_prices`, `hdo_optimized`.
- **Shared strategy modules** — Centralized pricing (`pricing.rs`), locking (`locking.rs`),
  and seasonal detection (`seasonal.rs`) modules extracted from strategies.
- **Strategy performance analysis documentation** — New docs covering strategy comparison
  results and cost calculation refactoring.
- **CLI simulator reference documentation** — Comprehensive guide for the `fluxion-sim` tool.

### Changed
- **Default strategy changed from V5 to V7** — V7 provides significantly better arbitrage
  performance: 62% better on volatile days, 87% better with negative prices, 48% better on
  usual days.
- **V3 strategy optimized** — Refactored V3 implementation with improved scheduling logic
  and HA plugin integration.
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

- **v0.2.36** (2026-02-15) - Mobile remote access via Tor, V10/V20/FPA strategies, heartbeat monitoring
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
