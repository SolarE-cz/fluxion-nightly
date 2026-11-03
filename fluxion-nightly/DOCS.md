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

## Network access

In the add-on, the web interface on port 8099 accepts requests only from the Home Assistant ingress proxy and from the add-on itself; hosts listed in `FLUXION_TRUSTED_PEERS` (comma-separated IP addresses) may also connect. Outside the add-on the listener binds to `127.0.0.1` unless `FLUXION_BIND_ADDR` is set (for example to `0.0.0.0`).

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

### Option: `system.trusted_peers`

Comma-separated IP addresses that may call the FluxION web interface (port 8099) besides the Home
Assistant ingress proxy and the add-on itself. Leave it empty unless another host on your network
must reach FluxION directly. Entries that are not IP addresses are ignored and logged as warnings.

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

#### Option: `inverters[].battery_extra_prefixes` and `inverters[].battery_extra_capacities_kwh`

For a parallel (master/slave) pair whose batteries FluxION sees through the master only: the entity
prefixes of the other units (for example `solax_81_jakub`) and the usable capacity in kWh of each
unit's battery, in the same order (each at least 0.1 kWh). The SOC this inverter reports is then the
capacity-weighted mean of its own battery and the other units' batteries. The other units are only
read, never written to.

The master's own share is `control.battery_capacity_kwh` minus the extra capacities, so
`control.battery_capacity_kwh` must be the pair's measured usable total (Jakub's pair: about 80 kWh,
not the 96 kWh nameplate), and each extra capacity that unit's measured share. Once the web UI has
saved its configuration, `control.battery_capacity_kwh` comes from the web UI (Settings), not from
the add-on option, so set the pair's total there.

Extras that do not validate (lengths differ, a capacity under 0.1 kWh, an inverter's own prefix, or
extras that leave the master no capacity) are ignored with a WARN in the log, and FluxION starts on
the master's own SOC.

An inverter (master or unit) is live while any of its heartbeat sensors was updated within 120 s.
The heartbeat sensors are `sensor.<prefix>_grid_voltage*`, `_grid_frequency*`, `_measured_power`,
`_inverter_power`, `_house_load`, `_pv_power_total` and `_battery_power_charge`, whichever exist;
the log lists them at start. A flat SOC does not count as stale: Home Assistant keeps its
`last_updated`, and its REST API does not advance `last_reported` while a value stays the same. An
inverter without any heartbeat sensor is not age-checked (one WARN). If Home Assistant cannot list
its entities, state reads fail until it can (one WARN).

A unit's SOC counts as unreadable when the read fails, the value is not a number, or the unit is not
live. A unit in standby is the exception: with its battery and PV power both within 50 W of 0 its
SOC cannot move, so it is used even with every heartbeat flat (a slave at its SOC floor at night).
Standby is believed only while the master shows at most 200 W of PV and is not charging (battery at
most +50 W); otherwise the unit would be moving too, so it counts as not live. FluxION holds the
last combined SOC for up to 10 minutes while a unit is unreadable; after that it takes no new
inverter state until the unit is readable again, rather than use the master's own SOC (several kWh
off the pair's). A master that is not live fails the state read too. The state's age counts from the
master's newest heartbeat. With no new data for over 120 s FluxION writes no planned mode, and ends
a running force charge, force discharge or HOLD once with the default mode (Self-Use, or Back Up in
the Back Up default). It re-enters a forced mode only after 60 s of good reads; when the exit is the
second or later within 60 minutes, only after 10 minutes of good reads. A unit outage outside
standby changes modes at the latest 120 s + 10 minutes + 120 s after the unit's last heartbeat. A
unit that fails while in standby is found once the master shows more than 200 W of PV or charges (by
the next morning at the latest), and modes change 10 minutes + 120 s after that. Set in the add-on
configuration, both also apply when the web UI has saved its own configuration; removing them in the
add-on UI clears them.

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
slots is derived as `a × spot + b` from today's prices; when that fit is not exact (within 0.01
CZK/kWh) nothing is filled. The last successful check is kept in `/data`: when the entity has no
prices of today at all (second day of an outage), a check at most 48 hours old is used instead and
the banner says "validated <date>" (a check of one day covers that day and the next only). The
add-on options `ote_fallback_enabled`, `price_alert_notification`, `tomorrow_prices_check_time`,
`battery_wear_cost_czk_per_kwh`, `battery_efficiency` and `pv_forecast_correction_enabled` always
apply from the add-on configuration, also on installs whose settings were saved in the web UI. The
entity's prices always win once it has them again.

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
the threshold. It writes nothing else: no work mode, Back Up minimum SOC, HotStandBy, battery awaken
or pre-wake, and no unlock on a lock event (the export limit write still unlocks a Locked inverter
first when `auto_unlock_inverter` is on). Planning, prices, alerts and the dashboard keep running;
the dashboard header shows "mode control off". The FluxION kill switch and the "Zakázat
Automatizaci" lever still stop curtailment. Curtailment needs a known export limit above 0 W to
restore: set the inverter's export limit above 0 W first. Set in the add-on configuration, it also
applies when the web UI has saved its own configuration. A change needs an add-on restart (saving
the add-on configuration restarts it); the web UI cannot change it while FluxION runs.

Default value: `true`

#### Option: `control.hold_pv_to_battery`

What HOLD does on this site. HOLD means the house is not served from the battery: by default
(`false`) the planner models it as PV serving the house, the surplus charging the battery and the
rest exported, and the battery never discharges. This is what direct VPP control's HOLD (mode 7)
does. Set `true` only for a site that drives modes through EEPROM and whose "Stop Charge and
Discharge" sends PV into the battery first while the house imports from the grid (measured on
Jakub's SolaX Ultra pair, 2026-10-26); the planner then models HOLD that way. With direct VPP
control the option is ignored. Set in the add-on configuration, it also applies when the web UI has
saved its own configuration.

Keep it `false` unless a replay on that site shows a gain. On Jakub's site, where it describes the
inverter correctly, it replayed about 2 CZK/day worse (10.7 days, October 2026). The default model
plans more midday HOLD. On that site, HOLD stores PV while the house imports, and the evening peaks
paid back more than those imports cost. The gap is within the replay's run-to-run spread of a few
CZK/day.

Default value: `false`

#### Option: `control.update_interval_secs`

How often (in seconds) FluxION checks conditions and updates control decisions. Must be between 10
and 3600 seconds.

Default value: `60`

### Option Group: `strategies.winter_reserve`

Only the cautious-PV options are described here; the other Winter Reserve options keep their
defaults unless a replay shows a gain.

#### Option: `strategies.winter_reserve.cautious_pv_mae_pct`

Cautious PV: while the solar forecast misses a lot, the planner plans on the forecast PV times
`pv_confidence` (default 0.70) instead of the forecast itself, so a sunny forecast that does not
come true no longer skips a cheap night charge. Otherwise `pv_confidence` only sizes the night
reserve.

The trigger is the over-forecast error of the forecast the plan used (PV forecast correction,
`solar_forecast.pv_forecast_correction_enabled`) over the last 7 complete days:
Σ max(forecast − measured, 0) / Σ max(forecast, measured), always 0–100 %. Only sun that was
forecast but did not come counts; more sun than forecast does not. It uses the same valid hours and
days as the correction: hours with missing data or curtailed PV and dim days are left out. Each
day's forecast is scaled by the correction factor the planner applied that day (the factor of the
day's recompute, or the last one it held; kept with the day's forecast snapshot), so a steady bias that the correction already removed does not
count, while a bias it had not caught yet does. Days without a stored factor (rebuilt from HA
history, or recorded by an earlier version) use the current factor. Cautious PV turns on when the error reaches this value with at least 5
valid days in the window, and stays on until the error drops below
`strategies.winter_reserve.cautious_pv_off_pct`. With fewer than 5 valid days the fallback months
decide. The state is kept across restarts and shown as the `cautious_pv` and
`cautious_pv_error_pct` attributes of `sensor.fluxion_solar_forecast_bias`. `100` turns the
trigger off. The value is read when the add-on starts.

Default value: `25.0`

#### Option: `strategies.winter_reserve.cautious_pv_off_pct`

Once on, cautious PV stays on while the over-forecast error is at or above this value (%). It must
not exceed `cautious_pv_mae_pct`. Read when the add-on starts.

Default value: `20.0`

#### Option: `strategies.winter_reserve.cautious_pv_fallback_months`

Local calendar months (1–12) planned on cautious PV while the forecast error does not decide: fewer
than 5 valid days, or the PV forecast correction off. An empty list turns the fallback off; with
`cautious_pv_mae_pct: 100` as well, the planner behaves as before this option.

Default value: `[10, 11, 12, 1, 2, 3]`

## How It Works

FluxION operates on a 15-minute time block schedule, analyzing electricity spot prices to determine
optimal battery charging and discharging strategies. The system:

1. **Monitors** real-time electricity prices and solar generation
2. **Analyzes** price trends to identify optimal charge/discharge windows
3. **Controls** your inverter to maximize economic benefit
4. **Adapts** to changing conditions and user preferences

The core principle is **profitability** - sometimes the most profitable action is to do nothing and
let the system run naturally.

**Learning period:** when history is configured (house-load power or consumption entity) but the
house-load profile covers fewer than 3 days, FluxION does not plan battery moves yet. Every block
runs the default mode (Self-Use, or Back Up when your default is Back Up) until 3 days of data are
in. Without a history entity this does not apply. User price and discharge levers take effect once
the learning period ends.

## Support

Got questions?

You could [open an issue][issue] on GitLab.

## Authors & Contributors

FluxION ECS is developed with a focus on maximizing user ROI from solar installations.

## License

This add-on is licensed under CC-BY-NC-ND-4.0 (Creative Commons
Attribution-NonCommercial-NoDerivatives 4.0 International).

[issue]: https://gitlab.com/SolarE-cz/fluxion/issues
