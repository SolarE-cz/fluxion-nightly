# Changelog

All notable changes to FluxION will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/), and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- Repository cleanup and documentation organization
- Comprehensive CONTRIBUTING.md with development guidelines
- CHANGELOG.md following Keep a Changelog format

### Changed

- Improved documentation structure (organized into docs/ subdirectories)
- Enhanced README.md with better Quick Start and Project Status sections
- Replaced TODO comments with descriptive "Future:" comments
- Improved documentation for future-use configuration methods

### Removed

- Dead code cleanup: removed ~801 lines of unused functions
- Obsolete configuration files and test data
- Outdated inventory files

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
