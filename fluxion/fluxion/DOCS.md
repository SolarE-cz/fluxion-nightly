# Home Assistant Add-on: FluxION ECS

FluxION ECS is an intelligent energy control system that optimizes your PV (photovoltaic) plant
operations based on electricity spot prices and real-time conditions.

## About

FluxION ECS automates your solar energy system to maximize profitability by intelligently managing
battery charging, discharging, and grid interactions based on real-time electricity prices. Built
with Rust and Bevy ECS architecture, it provides efficient, reliable control of your solar
installation.

**Key Features:**

- Multi-inverter support (Solax, with Fronius and SMA planned)
- Spot price-based optimization
- 15-minute time block scheduling
- Debug mode for safe testing
- Native Home Assistant integration
- Multi-language support (English, Czech)

## Installation

1. Click the Home Assistant My button below to open the add-on on your Home Assistant instance.

2. Click the "Install" button to install the add-on.

3. Configure the add-on (see Configuration section below).

4. Start the "FluxION ECS" add-on.

5. Check the logs to verify everything is working correctly.

## Configuration

**Note**: _Remember to restart the add-on when the configuration is changed._

Example add-on configuration:

```yaml
debug_mode: false
log_level: info
inverters:
  - id: "solax_1"
    vendor: "solax"
    entity_prefix: "solax"
    topology: "independent"
    min_battery_soc: 10
    max_battery_soc: 100
pricing:
  spot_price_entity: "sensor.current_spot_electricity_prices"
  use_spot_prices_to_buy: true
  use_spot_prices_to_sell: true
  force_charge_hours: 4
  force_discharge_hours: 2
  fixed_buy_prices: []
  fixed_sell_prices: []
control:
  maximum_export_power_w: 10000
  update_interval_secs: 60
```

### Option: `debug_mode`

When enabled, FluxION will run in simulation mode without actually controlling your inverters. This
is useful for testing your configuration and observing how FluxION would behave under different
conditions.

**Note**: _Always test your configuration in debug mode first!_

### Option: `log_level`

The `log_level` option controls the level of log output by the add-on and can be changed to be more
or less verbose, which might be useful when you are dealing with an unknown issue. Possible values
are:

- `trace`: Show every detail, like all called internal functions.
- `debug`: Shows detailed debug information.
- `info`: Normal (usually) interesting events.
- `warn`: Exceptional occurrences that are not errors.
- `error`: Runtime errors that do not require immediate action.

Please note that each level automatically includes log messages from a more severe level, e.g.,
`debug` also shows `info` messages. By default, the `log_level` is set to `info`, which is the
recommended setting unless you are troubleshooting.

### Option Group: `inverters`

Configure your solar inverters. FluxION supports multiple inverters with different topologies.

#### Option: `inverters[].id`

Unique identifier for this inverter. This ID will be used in logs and internal references.

#### Option: `inverters[].vendor`

The manufacturer of your inverter. Currently supported:

- `solax`: Solax inverters
- `fronius`: Fronius inverters (planned)
- `sma`: SMA inverters (planned)

#### Option: `inverters[].entity_prefix`

The prefix used for Home Assistant entities related to this inverter. For example, if your battery
capacity sensor is `sensor.solax_battery_capacity`, the prefix would be `solax`.

#### Option: `inverters[].topology`

Defines the inverter's role in multi-inverter setups:

- `independent`: Standalone inverter
- `master`: Master inverter in a master-slave configuration
- `slave`: Slave inverter controlled by a master

#### Option: `inverters[].min_battery_soc`

Minimum battery state of charge (%) that FluxION will maintain. The system will not discharge the
battery below this level.

Default value: `10`

#### Option: `inverters[].max_battery_soc`

Maximum battery state of charge (%) that FluxION will charge to.

Default value: `100`

### Option Group: `pricing`

Configure how FluxION obtains and uses electricity pricing information.

#### Option: `pricing.spot_price_entity`

The Home Assistant entity that provides current electricity spot prices. This should be a sensor
that updates with current market prices.

Example: `sensor.current_spot_electricity_prices`

#### Option: `pricing.use_spot_prices_to_buy`

Enable using spot prices to determine when to buy electricity from the grid (charge batteries when
prices are low).

Default value: `true`

#### Option: `pricing.use_spot_prices_to_sell`

Enable using spot prices to determine when to sell electricity to the grid (discharge batteries when
prices are high).

Default value: `true`

#### Option: `pricing.force_charge_hours`

Number of hours per day to force charge the battery during the lowest price periods.

Default value: `4`

#### Option: `pricing.force_discharge_hours`

Number of hours per day to force discharge the battery during the highest price periods.

Default value: `2`

#### Option: `pricing.fixed_buy_prices`

Optional list of fixed electricity buy prices if not using spot prices.

#### Option: `pricing.fixed_sell_prices`

Optional list of fixed electricity sell prices if not using spot prices.

### Option Group: `control`

Fine-tune FluxION's control behavior.

#### Option: `control.maximum_export_power_w`

Maximum power (in watts) that can be exported to the grid.

Default value: `10000`

#### Option: `control.update_interval_secs`

How often (in seconds) FluxION checks conditions and updates control decisions. Must be between 10
and 3600 seconds.

Default value: `60`

## How It Works

FluxION operates on a 15-minute time block schedule, analyzing electricity spot prices to determine
optimal battery charging and discharging strategies. The system:

1. **Monitors** real-time electricity prices and solar generation
2. **Analyzes** price trends to identify optimal charge/discharge windows
3. **Controls** your inverter to maximize economic benefit
4. **Adapts** to changing conditions and user preferences

The core principle is **profitability** - sometimes the most profitable action is to do nothing and
let the system run naturally.

## Support

Got questions?

You could [open an issue][issue] on GitLab.

## Authors & Contributors

FluxION ECS is developed with a focus on maximizing user ROI from solar installations.

## License

This add-on is licensed under CC-BY-NC-ND-4.0 (Creative Commons
Attribution-NonCommercial-NoDerivatives 4.0 International).

[issue]: https://gitlab.com/SolarE-cz/fluxion/issues
