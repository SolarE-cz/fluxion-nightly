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

#### Option: `inverters[].control_entity_prefix`

Optional prefix of the inverter's control entities (`select.*`, `number.*`, `button.*`, `switch.*`,
`time.*`) when the integration names them differently from the sensors. For example, with sensors
`sensor.solax_battery_capacity` and controls `select.solax_inverter_charger_use_mode`,
`select.solax_inverter_lock_state` and `number.solax_inverter_export_control_user_limit`, set
`entity_prefix: solax` and `control_entity_prefix: solax_inverter`. Sensors (`sensor.*`,
`binary_sensor.*`) always use `entity_prefix`.

Default: empty (control entities use the same prefix as before). The add-on log shows each
inverter's `entity prefix` and `control prefix` at startup. Set in the add-on configuration, it also
applies when the web UI has saved its own configuration.

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

#### Option: `pricing.tomorrow_prices_check_time`

Time of day (Europe/Prague, `HH:MM`) after which tomorrow's 15-minute prices are expected in the
spot price entity. If they are missing after this time (or prices for the rest of today are
missing), FluxION logs a warning once per hour and shows a banner on the dashboard.

Default value: `"13:45"`

#### Option: `pricing.ote_fallback_enabled`

When the spot price entity lacks prices, fetch the OTE day-ahead prices directly (converted to
CZK/kWh with the CNB EUR rate, like the Czech Energy Spot Prices integration) and fill only the
missing slots. The OTE prices are used only if they match the entity on at least 90 % of today's
slots (within 0.5 % or 0.01 CZK/kWh). With a separate buy price entity, the buy price of filled
slots is derived as `a × spot + b` from today's prices; when that fit is not exact (within
0.01 CZK/kWh) nothing is filled. The last successful check is kept in `/data`: when the entity has
no prices of today at all (second day of an outage), a check at most 48 hours old is used instead
and the banner says "validated <date>" (a check of one day covers that day and the next only).
The add-on options `ote_fallback_enabled`, `price_alert_notification`,
`tomorrow_prices_check_time`, `battery_wear_cost_czk_per_kwh`, `battery_efficiency` and
`pv_forecast_correction_enabled` always apply from the add-on configuration, also on installs
whose settings were saved in the web UI. The entity's prices always win once it has them again.

Default value: `true`

#### Option: `pricing.price_alert_notification`

Show one Home Assistant notification (`fluxion_prices_missing`) while prices are missing, and
dismiss it when they are complete again.

Default value: `true`

### Option Group: `control`

Fine-tune FluxION's control behavior.

#### Option: `control.maximum_export_power_w`

Maximum power (in watts) that can be exported to the grid.

Default value: `10000`

#### Option: `control.mode_control_enabled`

When `false`, FluxION only curtails export: while the sell price is at or below
`export_curtail_at_or_below_czk` (with `export_curtailment_enabled: true`) it sets the inverter's
export limit to `export_limit_min_w` and restores the limit it read before once the price is above
the threshold. It writes nothing else: no work mode, Back Up minimum SOC, HotStandBy, battery
awaken or pre-wake, and no unlock on a lock event (the export limit write still unlocks a Locked
inverter first when `auto_unlock_inverter` is on). Planning, prices, alerts and the dashboard keep
running; the dashboard header shows "mode control off". The FluxION kill switch and the
"Zakázat Automatizaci" lever still stop curtailment. Curtailment needs a known export limit above
0 W to restore: set the inverter's export limit above 0 W first. Set in the add-on configuration,
it also applies when the web UI has saved its own configuration. A change needs an add-on restart
(saving the add-on configuration restarts it); the web UI cannot change it while FluxION runs.

Default value: `true`

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
