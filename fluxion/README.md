# FluxION ECS - Home Assistant Add-on

Energy Control System for PV plant automation - Rust implementation.

## About

FluxION ECS is a Rust-based energy control system that optimizes your PV (photovoltaic) plant
operations through intelligent automation. It integrates seamlessly with Home Assistant to provide
real-time control and monitoring of your solar energy system.

## Features

- **Multi-inverter Support**: Works with Solax inverters (with support for Fronius and SMA planned)
- **Spot Price Integration**: Automatically adjusts energy usage based on current electricity prices
- **15-minute Time Block Scheduling**: Fine-grained control over energy management
- **Debug Mode**: Safe testing environment without affecting actual hardware
- **ECS Architecture**: Built on Bevy ECS for efficient, modular system design
- **Home Assistant Integration**: Native integration with Home Assistant ecosystem

## Installation

1. Add this repository to Home Assistant:

   - Navigate to: Settings → Add-ons → Add-on Store → ⋮ (menu) → Repositories
   - Add URL: `https://gitlab.com/your-org/fluxion`

2. Find "FluxION ECS" in the add-on store and click Install

3. Configure the add-on (see Configuration section below)

4. Start the add-on

5. Check the logs to verify everything is working correctly

## Configuration

The add-on is configured through Home Assistant's UI. Here's an example configuration:

```yaml
debug_mode: false
log_level: info
inverters:
  - type: solax
    host: 192.168.1.100
    serial_number: YOUR_SERIAL
    register_prefix: 0
pricing:
  provider: spot_price_api
  api_url: https://api.example.com/prices
  update_interval: 900
control:
  max_battery_soc: 100
  min_battery_soc: 10
  time_blocks: 96
```

### Configuration Options

#### General Settings

- `debug_mode` (boolean, optional): Enable debug mode for testing without affecting hardware.
  Default: `false`
- `log_level` (string, optional): Logging verbosity. Options: `error`, `warn`, `info`, `debug`,
  `trace`. Default: `info`

#### Inverters

Configure your solar inverters:

- `type` (string, required): Inverter type. Currently supported: `solax`
- `host` (string, required): IP address or hostname of the inverter
- `serial_number` (string, required): Serial number of the inverter
- `register_prefix` (integer, optional): Modbus register prefix. Default: `0`

#### Pricing

Configure electricity pricing data:

- `provider` (string, required): Pricing data provider
- `api_url` (string, required): API endpoint for price data
- `update_interval` (integer, optional): How often to fetch prices (in seconds). Default: `900` (15
  minutes)

#### Control Settings

Fine-tune energy control parameters:

- `max_battery_soc` (integer, optional): Maximum battery state of charge (%). Default: `100`
- `min_battery_soc` (integer, optional): Minimum battery state of charge (%). Default: `10`
- `time_blocks` (integer, optional): Number of time blocks per day. Default: `96` (15-minute blocks)

## Support

For issues, feature requests, or contributions, please visit the
[GitLab repository](https://gitlab.com/your-org/fluxion).

## License

MIT OR Apache-2.0
