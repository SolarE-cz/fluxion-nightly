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
Assistant ingress proxy and the add-on itself. Leave it empty. Only containers on the Home Assistant
add-on network (no ports are exposed) can reach port 8099. Entries that are not IP addresses are
ignored and logged as warnings.

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
the Back Up default). With direct VPP control that end is VPP mode 0, followed by the default work
mode only when the inverter's work mode (last read) differs from it; no VPP command is renewed while
the data is stale, so a Feed-in or curtailment mode 8 lapses by its own 3-minute duration. It
re-enters a forced mode only after 60 s of good reads; when the exit is the second or later within
60 minutes, only after 10 minutes of good reads. A unit outage outside standby changes modes at the
latest 120 s + 10 minutes + 120 s after the unit's last heartbeat. A unit that fails while in
standby is found once the master shows more than 200 W of PV or charges (by the next morning at the
latest), and modes change 10 minutes + 120 s after that. Set in the add-on configuration, both also
apply when the web UI has saved its own configuration; removing them in the add-on UI clears them.

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
Jakub's SolaX Ultra pair, 2026-10-26); the planner then models HOLD that way. When this run uses
direct VPP control (the decided path, see `control.vpp_direct_path`) the option is ignored and the
log says so once (INFO). Set in the add-on configuration, it also applies when the web UI has saved
its own configuration.

Keep it `false` unless a replay on that site shows a gain. On Jakub's site, where it describes the
inverter correctly, it replayed about 2 CZK/day worse (10.7 days, October 2026). The default model
plans more midday HOLD. On that site, HOLD stores PV while the house imports, and the evening peaks
paid back more than those imports cost. The gap is within the replay's run-to-run spread of a few
CZK/day.

#### Option: `control.vpp_direct_enabled`

SolaX only. Default: `true`. Every block mode and export curtailment goes through the direct VPP
remote control: Force Charge, Force Discharge, Hold, Feed-in, Self Use under Back Up, and export curtailment
are written to `select.<control prefix>_modbus_power_control_direct` and
`number.<control prefix>_remotecontrol_*_direct`. These writes are volatile and re-sent about every
60 s; FluxION reads the state back from `sensor.<prefix>_modbus_power_control`. The only EEPROM write
is the basic work mode (`select.<prefix>_charger_use_mode`, Self Use or Back Up from the default
mode).

Requires solax-modbus 2026.10.01 or newer. Version 2026.09.2 writes the registers one by one and is
not usable. The version is read from the integration's HACS update entity
(`update.solax_inverter_modbus_update`, or any `update.*` naming solax and modbus); without it,
or with a version that is older or not parsable, FluxION does not use direct VPP (fail closed).
The direct entities must exist. Otherwise FluxION falls back to the EEPROM mode writes
(the per-block work mode and export limit writes) and logs a WARN once: "Direct VPP unavailable:
<reason>, using EEPROM mode writes". The same fallback runs for a non-SolaX inverter and with mode
control off.

The startup check needs one complete read of Home Assistant (states readable, the integration
loaded, its version known). It is retried every 10 s for up to 120 s, and no mode or export
limit is written on either path until it decides. Without a complete read in 120 s the EEPROM
mode writes run (WARN) and the check is retried every 60 s, reading only the integration's
update entity and the direct entities; a later complete read that passes switches to direct VPP
once (one way, logged). The 120 s are wall clock: a read still hanging then is cut off. A complete read decides for the whole run.
`/api/config` shows the path of this run in `control.vpp_direct_path` ("deciding (...)",
"direct VPP", "direct VPP (modes 1-7, no mode 8: <inverter>)" or "EEPROM mode writes
(<reason>)"); `control.vpp_direct_enabled` always shows the configured value. Set the
option to `false` to force the EEPROM mode writes. Both target-set-type selects (`select.<control prefix>_remote_control_target_set_type_direct`,
`select.<control prefix>_power_control_mode_target_set_type_direct`) must be "Set", since the
keep-alive relies on a re-sent command restarting its duration. At startup FluxION sets them to
"Set" while the power control reads Disabled (debug mode only logs "[DEBUG] Would set target
type Set"); under a running command it does not change them and falls back to the EEPROM mode
writes (WARN). A check that ran in debug mode (at start, or debug switched while it was pending)
leaves the type unverified: it is then set before the first VPP command once debug mode is off.
If the type cannot be set under a running command, the EEPROM mode writes take over (WARN); a
read, connection or unlock error is retried every 10 s, with no VPP command meanwhile. If the type
is still not verified 120 s after the first check (e.g. the read-back stays unavailable), the
EEPROM mode writes take over for the rest of the run (ERROR); a kill switch turned on meanwhile
gets its mode 0 first, and if the check passes in time, its mode 0 and default mode follow then.

A non-zero VPP mode left by an earlier run (e.g. a crash) is ended with one mode 0 before the
first plan.

The integration re-sends the whole command when a parameter of the active mode changes, so FluxION
writes only changed parameters, in an order where each step is safe (mode 8: the push first when it
falls or the PV limit rises, else the PV limit first; a switch between curtailing and an uncapped
PV limit of 30 000 W writes the PV limit first both ways), and the mode select on a mode change, as the
keep-alive, at least every 50 s, and when the read-back shows another mode. If the integration
rejects a parameter as unknown (a VPP mode started outside FluxION, or first use after an upgrade),
FluxION writes mode 0, all parameters and the mode once more (WARN). Any other write error (e.g. the
integration cannot read the active mode over Modbus) never drops to mode 0: the command is re-sent
after 20 s. Only the newest pending command per inverter is queued. A command that lowers export
(a lower push, or a lower PV limit + push in mode 8) is written at once, not held by the 20 s gap,
as is a PV limit raised because the house imports.

On stop (kill switch, automation lever, debug mode, add-on stop, or mode control off) FluxION writes
VPP mode 0, which exits to the basic work mode. The kill switch and the automation lever then also
write the configured default mode (e.g. Back Up with its minimum SOC), once per activation; a mode
changed by hand after that is left alone. When the kill switch is already on at start, FluxION
writes mode 0 once if a VPP command is live (it waits up to 120 s for a known read-back) and never
writes the basic work mode, so a mode set by hand stays. On the EEPROM path it writes no mode at
all until the switch is turned off (a VPR still runs, and Self Use follows its end).

**Parallel (master/slave) sites** use direct VPP like any other site. Commands go to the master
only; the slaves follow it in the same read-back poll (measured on an X3-Ultra pair, 2026-10-28).
The push power is the system total, shared between the units (no multiplication by the unit count),
and the discharge clamp uses the site's PV (master plus `pv_extra_prefixes`). FluxION never writes a
slave's direct entities.

**Inverters without mode 8** (the X3-Ultra, GEN5: solax-modbus 2026.10.x has no mode 8/9 entities
for it) are detected at startup and run modes 1–7 (logged; `control.vpp_direct_path` says so):
Force Charge is mode 4 push −W, Force Discharge mode 4 push +W (the discharge regime below), Feed-in mode 4
push 0, Hold mode 7, Self Use under Back Up mode 6. Mode 4 lapses to mode 6 after about 10 minutes
without a write, so the 60 s keep-alive applies. Export curtailment is mode 7 while the battery can
take the surplus; when mode 8 would be needed (full battery, surplus above the charge power, Force
Charge or Force Discharge during curtailment) the export limit curtails as with the option off, for
the rest of that curtailment (logged once per inverter as "no mode 8 on this inverter"), and is
restored when it ends.

How each plan mode is sent (basic work mode = the default mode; without mode 8 see above):

| Plan mode | Direct VPP command |
|---|---|
| Self Use (basic Self Use) / Back Up (basic Back Up) | mode 0: the basic work mode runs |
| Self Use while the basic mode is Back Up | mode 6 (self-consume charge/discharge) |
| Back Up while the basic mode is Self Use (VPR "no discharge", fixed slot) | mode 7 (no discharge, PV charges) |
| Force Charge | mode 8, PV uncapped, push = −`max_battery_charge_rate_kw`; mode 7 at `max_battery_soc` |
| Force Discharge | mode 8, two regimes (below); mode 7 at the discharge floor |
| Hold (Stop Charge and Discharge) | mode 7 (PV surplus to the battery, never discharges) |
| Feed-in Priority | mode 8, push 0, the same two regimes: battery idle, PV to house and grid (PV above the export cap is not stored) |

Force Discharge and Feed-in run in two regimes, stepped once per state read:
- **Discharge** while PV is below `maximum_export_power_w` + house load: PV uncapped (30 000 W), so
  the measured PV is the sun, and push = export cap + house load − PV within the battery (Feed-in:
  push 0). PV − load is estimated from the reads: an average for the level, the read noise, and
  a rising slope over the last 6–24 reads, used only when it stands out from the noise and then
  projected about 35 s ahead (the read-back lag and one read), so a push written now still fits
  when it applies. The push keeps 1.5 σ of the estimate below the cap. It falls at once (not held
  by the 20 s write gap, even by 100 W) when the last read or the mean of the last 3 shows export
  more than 100 W over the cap beyond the read noise, and it rises only beyond
  max(min(200 W, max(100 W, 4 σ)), 10 % of the load), so read noise writes nothing.
- **Curtail** once the push is 0 and PV is at or above cap + load for 2 reads (at once when 100 W
  over): PV limit = cap + house load (rounded up), push 0. The limit follows a load drop of 200 W,
  a rise beyond max(300 W, 10 %) or a rise of 100 W held for 30 s, and a grid import at once. It
  returns to the discharge regime when the mean of 3 settled PV reads (30 s after the last limit
  change) is below cap + load by max(300 W, 5 % of the limit + 100 W) plus the read noise, for 2
  reads. The 5 % covers the inverter's clip below its limit (measured 150 W at 3 kW).

Known limits, measured in a closed-loop plant (lag 15/30 s, ±200 W read noise, 5 seeds): without
noise, export is over the cap + 100 W only within the lag. With noise, a slow PV ramp (2–15 W/s)
is not told from the noise at once: export can stay up to ~925 W over (15 W/s) and up to 165 s
beyond the lag (2 W/s, about 300 W over); a cloud edge right after a regime switch can add up to
15 s. Cloud edges every 30–90 s each need a write, so they write more than 8 times per 15 min.

With PV or the house load unknown the PV limit is 30 000 W and the inverter's own export limit caps
the meter. Without mode 8 (mode 4) PV cannot be capped: the discharge regime only, with the same
push estimate and bands, and the inverter's export limit caps the meter (issue #649 Q3). Force Charge, Force Discharge and Feed-in use mode 8 (Force Charge PV limit 30 000 W,
clamped to the entity's maximum; duration 180 s, then back to the basic work mode) so that a crashed or stopped FluxION
lapses to the basic work mode within 3 minutes. Mode 8 discharge is not yet measured on hardware:
check it in an attended test before enabling the option on a site.

A command whose read-back (`sensor.<prefix>_modbus_power_control`) disagrees is counted at most
once a minute (an unavailable sensor is not counted). After two counts it is re-sent (WARN); after
five FluxION falls back to mode 0 and logs an ERROR; export curtailment on that inverter uses the
export limit again. Every 15 minutes FluxION retries the plan (logged), with the mismatch count
started over; the export limit keeps curtailing until the read-back confirms a VPP command. Toggling
the kill switch or debug mode also retries at once.

Export curtailment replaces the export limit write:

- **Mode 7** (charge only) while the battery can take the whole PV surplus: the inverter's own
  zero-export control puts the surplus into the battery and holds the meter at 0 W while the house
  load moves.
- **Mode 8** only when mode 7 would export: the battery at `max_battery_soc` (and a PV surplus
  above 100 W), or a PV surplus above the battery's charge power. PV limit = house load + charge,
  rounded up to 100 W (a slight export rather than an import), battery push = −charge. It is
  recomputed every state read and re-sent at most every 10 s when it changed (other commands: 20 s).
  Mode 8 does not follow the load between writes, so load swings show on the meter (a few hundred
  W for up to one cycle).
- Mode 8 goes back to mode 7 after two state reads with PV below the last sent limit by more than
  max(300 W, 10 %) (sun-limited; at full battery this means PV below the house load), not counting
  reads within 30 s of a raised limit (PV still ramping), or, at full battery, once the SOC is 1 %
  below `max_battery_soc`. The inverter clips about 150 W below the limit (measured 3000 W → 2.85
  kW), so at full battery the meter can show a small import of that size.
- Force Charge during curtailment: mode 8 with the full charge power (the grid tops up PV). Force
  Discharge: mode 8 with the battery idle (no discharge into a curtailed grid).

Mode 8 is the only volatile PV limiter: mode 1 cannot throttle PV (per the integration authors), so
it is not used.

Curtailment also runs before the first plan. The inverter's persistent export limit is not relied
on. Untested on hardware so far: mode 7 with a PV surplus above the charge power (the switch to mode
8 covers it). Force Discharge is capped so that export stays under `maximum_export_power_w`.

Set in the add-on configuration. A change needs an add-on restart; the web UI cannot change it while
FluxION runs.

Default value: `false`

#### Option: `control.update_interval_secs`

How often (in seconds) FluxION checks conditions and updates control decisions. Must be between 10
and 3600 seconds.

Default value: `60`

### Option Group: `strategies.winter_reserve`

Only the cautious-PV options and the direction-reversal cost are described here; the other Winter
Reserve options keep their defaults unless a replay shows a gain.

#### Option: `strategies.winter_reserve.direction_reversal_cost_czk`

Planning cost (CZK) of reversing the battery's force direction: a grid Charge after an Export
(Force Discharge), or an Export after a grid Charge, with any Self-Use or Hold blocks between them.
The planner switches direction only where that gains at least this much, so on a flat buy price
charge and export run in contiguous blocks instead of alternating every 15 minutes (each switch is
an inverter mode write). The planner still cycles as often as the spread pays for, and exports in
the dearest blocks of a run. The cost only steers the plan; it is never added to the plan cost or
profit. It is a difference, not a price threshold. `0` turns it off (the plan before this option).
This option applies over a saved web UI configuration (`/data/config.json`).

Default value: `2.0`

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
