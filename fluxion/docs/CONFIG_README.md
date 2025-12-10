# FluxION Configuration Guide

Complete reference for configuring FluxION battery automation system.

## Table of Contents

- [Configuration File Locations](#configuration-file-locations)
- [Configuration Format](#configuration-format)
- [Inverters Configuration](#inverters-configuration)
- [Pricing Configuration](#pricing-configuration)
- [Control Configuration](#control-configuration)
- [System Configuration](#system-configuration)
- [Strategies Configuration](#strategies-configuration)
- [Environment Variables](#environment-variables)
- [Complete Examples](#complete-examples)
- [Validation Rules](#validation-rules)
- [Troubleshooting](#troubleshooting)

## Configuration File Locations

FluxION loads configuration from the first available source in this order:

1. **`/data/options.json`** - Home Assistant addon options (JSON format, auto-generated)
2. **`config.toml`** - Local TOML configuration file (recommended for development)
3. **`config.json`** - Local JSON configuration file
4. **Environment variables** - Can override specific settings
5. **Built-in defaults** - Safe defaults with debug mode enabled

## Configuration Format

FluxION supports two formats:

- **TOML** (recommended) - Human-friendly, supports comments
- **JSON** - Machine-friendly, used by Home Assistant addon

All examples below use TOML format. For JSON equivalent, remove comments and convert to JSON syntax.

## Inverters Configuration

Define one or more inverters. At least one inverter is required.

### Basic Inverter

```toml
[[inverters]]
id = "main_inverter"           # Unique identifier for this inverter
vendor = "solax"                # Inverter brand: solax, fronius, sma
entity_prefix = "solax"         # Home Assistant entity prefix
topology = "independent"        # Topology: independent, master, slave
```

### Configuration Fields

#### `id` (required, string)

- Unique identifier for this inverter
- Used internally to reference this inverter
- Example: `"main_inverter"`, `"master_inv"`, `"slave_1"`

#### `vendor` (required, string)

- Inverter manufacturer/brand
- Supported values:
  - `"solax"` - Solax Power inverters (tested with X3-Hybrid G4)
  - `"fronius"` - Fronius inverters (planned)
  - `"sma"` - SMA inverters (planned)

#### `entity_prefix` (required, string)

- Prefix used for Home Assistant entities
- FluxION will look for entities like `sensor.{prefix}_battery_soc`
- Must match your Home Assistant integration's entity naming
- Example: if your HA has `sensor.solax_battery_soc`, use `entity_prefix = "solax"`

#### `topology` (required, string)

- Defines how this inverter relates to others
- Values:
  - `"independent"` - Single inverter or multiple independent inverters
  - `"master"` - Controls one or more slave inverters
  - `"slave"` - Controlled by a master inverter

### Multi-Inverter Topologies

#### Multiple Independent Inverters

Each inverter operates independently:

```toml
[[inverters]]
id = "inverter_1"
vendor = "solax"
entity_prefix = "solax_1"
topology = "independent"

[[inverters]]
id = "inverter_2"
vendor = "solax"
entity_prefix = "solax_2"
topology = "independent"
```

#### Master/Slave Configuration

One master coordinates multiple slaves:

```toml
[[inverters]]
id = "master_inv"
vendor = "solax"
entity_prefix = "solax_master"
topology = "master"
slaves = ["slave_1", "slave_2"]    # List of slave inverter IDs

[[inverters]]
id = "slave_1"
vendor = "solax"
entity_prefix = "solax_slave1"
topology = "slave"
master = "master_inv"              # Reference to master inverter ID

[[inverters]]
id = "slave_2"
vendor = "solax"
entity_prefix = "solax_slave2"
topology = "slave"
master = "master_inv"
```

## Pricing Configuration

Configure electricity pricing for optimization decisions.

```toml
[pricing]
# Home Assistant sensor providing current spot electricity price
spot_price_entity = "sensor.current_spot_electricity_price_15min"

# Use real-time spot prices for buying decisions
use_spot_prices_to_buy = true

# Use real-time spot prices for selling decisions
use_spot_prices_to_sell = true

# Fixed hourly prices (24 values) - used as fallback when spot prices unavailable
fixed_buy_prices = [
    0.05, 0.05, 0.05, 0.05, 0.05, 0.05,  # 00:00-05:59 (night)
    0.06, 0.07, 0.08, 0.08, 0.07, 0.06,  # 06:00-11:59 (morning)
    0.06, 0.07, 0.08, 0.08, 0.09, 0.10,  # 12:00-17:59 (afternoon)
    0.09, 0.08, 0.07, 0.06, 0.05, 0.05   # 18:00-23:59 (evening)
]

fixed_sell_prices = [
    0.08, 0.08, 0.08, 0.08, 0.08, 0.08,  # 00:00-05:59
    0.09, 0.10, 0.11, 0.11, 0.10, 0.09,  # 06:00-11:59
    0.09, 0.10, 0.11, 0.11, 0.12, 0.13,  # 12:00-17:59
    0.12, 0.11, 0.10, 0.09, 0.08, 0.08   # 18:00-23:59
]
```

### Configuration Fields

#### `spot_price_entity` (required, string)

- Home Assistant entity ID that provides current electricity spot price
- Common examples:
  - `"sensor.current_spot_electricity_price_15min"`
  - `"sensor.nordpool_kwh_fi_eur_3_10_0"`
  - `"sensor.spot_price_kwh"`
- Must provide price in currency per kWh

#### `use_spot_prices_to_buy` (required, boolean)

- `true` - Use real-time spot prices for charging decisions
- `false` - Use fixed_buy_prices for charging decisions

#### `use_spot_prices_to_sell` (required, boolean)

- `true` - Use real-time spot prices for discharge/export decisions
- `false` - Use fixed_sell_prices for discharge decisions

#### `fixed_buy_prices` (required, array of floats)

- Fallback prices for buying electricity (charging battery)
- Prices in your local currency per kWh (e.g., CZK/kWh, EUR/kWh)
- Can provide:
  - **24 values** - one price per hour (will be expanded to 96 15-minute blocks)
  - **96 values** - one price per 15-minute block (for precise control)
- Used when:
  - `use_spot_prices_to_buy = false`
  - Spot price sensor is unavailable
  - Spot price data is stale

#### `fixed_sell_prices` (required, array of floats)

- Fallback prices for selling electricity (discharging battery or exporting to grid)
- Same format as `fixed_buy_prices`
- Typically higher than buy prices (feed-in tariff)

### Pricing Strategy Notes

**Using spot prices** (recommended):

- Enable both `use_spot_prices_to_buy` and `use_spot_prices_to_sell`
- FluxION will optimize based on real-time price fluctuations
- Best for markets with volatile spot prices (e.g., Nordic countries)

**Using fixed prices**:

- Disable spot prices and rely on `fixed_buy_prices`/`fixed_sell_prices`
- Useful for:
  - Fixed-rate contracts
  - Time-of-use (TOU) tariffs
  - Markets without spot price access

**Hybrid approach**:

- Enable spot for buying, disable for selling (or vice versa)
- Useful if you have fixed feed-in tariff but variable purchase prices

## Control Configuration

Battery control parameters and operational limits.

```toml
[control]
# Maximum power to export to grid (watts)
maximum_export_power_w = 5000

# Number of cheapest price periods to force battery charging
force_charge_hours = 4

# Number of most expensive price periods to force battery discharging
force_discharge_hours = 2

# Minimum battery state of charge for strategy decisions (%)
min_battery_soc = 15.0

# Maximum battery state of charge (%)
max_battery_soc = 100.0

# Hardware minimum SOC enforced by inverter firmware (%)
hardware_min_battery_soc = 10.0

# Battery capacity in kWh
battery_capacity_kwh = 23.0

# Battery wear cost per kWh cycled (currency/kWh)
battery_wear_cost_czk_per_kwh = 0.125

# Battery round-trip efficiency (0.0 to 1.0)
battery_efficiency = 0.95

# Minimum time between mode changes (seconds)
min_mode_change_interval_secs = 300

# Average household power consumption (kW) - fallback for predictions
average_household_load_kw = 0.5

# Minimum consecutive 15-minute blocks for force operations
min_consecutive_force_blocks = 2
```

### Configuration Fields

#### `maximum_export_power_w` (required, integer)

- Maximum power allowed to export to grid in watts
- Typical values: 3000-10000W depending on:
  - Your grid connection capacity
  - Inverter maximum export rating
  - Utility company limits
- Example: `5000` = max 5 kW export to grid

#### `force_charge_hours` (required, integer)

- How many of the cheapest price periods to force battery charging
- Range: 0-24
- `0` = no forced charging (battery only charges from solar)
- `4` = charge during the 4 cheapest hours of the day
- Higher values = more aggressive charging from grid

#### `force_discharge_hours` (required, integer)

- How many of the most expensive price periods to force battery discharging
- Range: 0-24
- `0` = no forced discharging (battery only used for self-consumption)
- `2` = discharge during the 2 most expensive hours
- Higher values = more aggressive arbitrage trading

#### `min_battery_soc` (required, float, 0-100)

- Minimum battery state of charge for strategy decisions
- This is the target minimum - strategies will try to keep SOC above this
- Typical values: 10-20%
- Should be >= `hardware_min_battery_soc`
- Example: `15.0` = strategies avoid discharging below 15%

#### `max_battery_soc` (required, float, 0-100)

- Maximum battery state of charge
- Upper limit for charging operations
- Typical values: 90-100%
- Setting below 100% can extend battery lifespan
- Example: `100.0` = allow charging to full capacity

#### `hardware_min_battery_soc` (required, float, 0-100)

- Absolute minimum SOC enforced by inverter firmware
- Read from inverter settings (e.g., Solax: `number.solax_selfuse_discharge_min_soc`)
- Cannot be overridden by FluxION
- Default: `10.0`
- Should be \<= `min_battery_soc`

#### `battery_capacity_kwh` (required, float)

- Total battery capacity in kilowatt-hours
- Used for:
  - SOC predictions
  - Economic calculations
  - Wear cost estimates
- Example: `23.0` for a 23 kWh battery system
- Check your battery specifications for accurate value

#### `battery_wear_cost_czk_per_kwh` (required, float)

- Cost of battery degradation per kWh cycled
- In your display currency per kWh
- Used for economic optimization (wear cost vs. arbitrage profit)
- Calculation example:
  - Battery cost: 115,000 CZK
  - Capacity: 23 kWh
  - Cycle life: 6,000 cycles
  - Cost per cycle: 115,000 / 6,000 = 19.17 CZK
  - Cost per kWh: 19.17 / 23 = 0.833 CZK/kWh full cycle
  - Conservative estimate: ~0.125 CZK/kWh (accounting for partial cycles)
- Default: `0.125`

#### `battery_efficiency` (required, float, 0.0-1.0)

- Battery round-trip efficiency (charge-discharge cycle)
- Accounts for energy losses in battery and inverter
- Typical lithium-ion: 0.90-0.95 (90-95%)
- Example: `0.95` = 5% energy loss per cycle
- Used for economic calculations

#### `min_mode_change_interval_secs` (required, integer)

- Minimum time in seconds between inverter mode changes
- Prevents rapid switching that can:
  - Wear out inverter EEPROM
  - Cause instability
  - Reduce efficiency
- Minimum allowed: 60 seconds (1 minute)
- Default: 300 seconds (5 minutes)
- Increase if you see frequent mode switching

#### `average_household_load_kw` (required, float)

- Average household power consumption in kilowatts
- Used as fallback when actual load sensor unavailable
- Typical values:
  - Small household: 0.3-0.5 kW
  - Medium household: 0.5-0.8 kW
  - Large household: 0.8-1.5 kW
- Default: `0.5` kW (500W)
- FluxION prefers actual load sensor if available

#### `min_consecutive_force_blocks` (required, integer)

- Minimum number of consecutive 15-minute blocks for force operations
- Prevents single-block force operations that cause excessive inverter writes
- Values:
  - `1` = allow single 15-minute blocks (not recommended)
  - `2` = minimum 30 minutes (default, recommended)
  - `4` = minimum 1 hour (more conservative)
- Default: `2`

## System Configuration

System-wide settings and Home Assistant connection.

```toml
[system]
# Debug mode - logs actions without making hardware changes
debug_mode = true

# Update interval in seconds (how often to run control loop)
update_interval_secs = 60

# Log level (error, warn, info, debug, trace)
log_level = "info"

# Display currency for web UI (EUR, USD, CZK)
display_currency = "CZK"

# User interface language (en, cs)
language = "en"

# Home Assistant connection (optional - auto-detected in addon mode)
# ha_base_url = "http://homeassistant.local:8123"
# ha_token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
```

### Configuration Fields

#### `debug_mode` (required, boolean)

- Safe mode that logs actions without making hardware changes
- **Default: `true` for safety**
- Values:
  - `true` - Simulate control actions, log what would happen (SAFE)
  - `false` - Make real changes to inverter settings (PRODUCTION)
- **Important**: Always test with `debug_mode = true` first
- Only set to `false` after:
  - Verifying configuration
  - Testing in debug mode
  - Understanding the control strategies

#### `update_interval_secs` (required, integer)

- How often FluxION runs its control loop (in seconds)
- Minimum: 10 seconds
- Recommended: 60 seconds (1 minute)
- Typical values:
  - `60` - good balance of responsiveness and load
  - `30` - more responsive, higher system load
  - `120` - less load, slower to react to changes
- Higher values:
  - Reduce system load
  - Use less network bandwidth
  - Slower reaction to price/condition changes

#### `log_level` (required, string)

- Controls verbosity of logging output
- Values (from least to most verbose):
  - `"error"` - Only critical errors
  - `"warn"` - Errors and warnings
  - `"info"` - Normal operation info (recommended)
  - `"debug"` - Detailed debugging information
  - `"trace"` - Extremely verbose (development only)
- Use `"debug"` for troubleshooting
- Use `"info"` for normal operation

#### `display_currency` (required, string)

- Currency used in web UI and logs
- Values:
  - `"EUR"` - Euros
  - `"USD"` - US Dollars
  - `"CZK"` - Czech Crowns
- Affects display only, not calculations
- Match this to your spot price entity's currency

#### `language` (required, string)

- User interface language
- Supported values:
  - `"en"` - English
  - `"cs"` - Czech (Čeština)
- Default: `"en"`
- Affects web UI, logs, and messages

#### `ha_base_url` (optional, string)

- Home Assistant base URL
- **Only needed when running outside Home Assistant addon**
- Format: `"http://hostname:port"` or `"https://hostname:port"`
- Examples:
  - `"http://homeassistant.local:8123"`
  - `"http://192.168.1.100:8123"`
  - `"https://ha.example.com"`
- Not needed in addon mode (auto-detected)

#### `ha_token` (optional, string)

- Home Assistant long-lived access token
- **Only needed when running outside Home Assistant addon**
- Get token from HA:
  1. Log into Home Assistant
  2. Click your profile (bottom left)
  3. Scroll to "Long-Lived Access Tokens"
  4. Click "Create Token"
  5. Copy the token
- Security warning: Never commit tokens to git
- Not needed in addon mode (uses `SUPERVISOR_TOKEN` automatically)

#### `timezone` (optional, string, auto-detected)

- System timezone (e.g., "Europe/Prague")
- **Auto-detected from Home Assistant at startup**
- Normally not configured manually
- Used for scheduling and time-based strategies

## Strategies Configuration

Fine-tune optimization strategies.

```toml
[strategies.winter_peak_discharge]
# Enable winter peak discharge strategy
enabled = true

# Minimum price spread required to trigger discharge (currency/kWh)
min_spread_czk = 3.0

# Minimum SOC required to start discharge (%)
min_soc_to_start = 70.0

# Target minimum SOC after discharge (%)
min_soc_target = 50.0

# Solar production window start hour (0-23)
solar_window_start_hour = 9

# Solar production window end hour (0-23)
solar_window_end_hour = 15

# Minimum hours before solar window to allow discharge
min_hours_to_solar = 4

[strategies.solar_aware_charging]
# Enable solar-aware charging strategy
enabled = true

# Solar production window start hour (0-23)
solar_window_start_hour = 9

# Solar production window end hour (0-23)
solar_window_end_hour = 12

# Maximum SOC at midday to leave room for solar (%)
midday_max_soc = 90.0

# Minimum solar forecast to enable this strategy (kWh)
min_solar_forecast_kwh = 2.0

[strategies.seasonal]
# Force specific season (optional)
# force_season = "winter"  # Options: "winter", "summer"
```

### Winter Peak Discharge Strategy

Optimizes battery discharge during expensive evening peaks while preserving enough charge for
overnight needs.

#### `enabled` (boolean, default: true)

- Enable/disable this strategy
- `true` - Strategy participates in decisions
- `false` - Strategy is inactive

#### `min_spread_czk` (float, default: 3.0)

- Minimum price difference (currency/kWh) between peak and average to trigger discharge
- Higher values = more conservative (only discharge on large price spikes)
- Lower values = more aggressive (discharge on smaller spreads)
- Prevents discharge when arbitrage gain is too small

#### `min_soc_to_start` (float, default: 70.0, range: 0-100)

- Minimum battery SOC (%) required to start peak discharge
- Strategy won't discharge if battery is below this level
- Ensures sufficient charge is available before aggressive discharge

#### `min_soc_target` (float, default: 50.0, range: 0-100)

- Target minimum SOC (%) after discharge completes
- Strategy aims to keep SOC above this during discharge
- Should be less than `min_soc_to_start`

#### `solar_window_start_hour` (integer, default: 9, range: 0-23)

- Hour when solar production typically begins
- Strategy avoids discharging too close to solar hours

#### `solar_window_end_hour` (integer, default: 15, range: 0-23)

- Hour when significant solar production typically ends
- Defines the solar production window

#### `min_hours_to_solar` (integer, default: 4)

- Minimum hours before solar window required to allow discharge
- Prevents draining battery right before solar production begins
- Example: if solar starts at 9:00 and this is 4, no discharge after 5:00

### Solar-Aware Charging Strategy

Avoids charging battery from grid right before expected solar production, leaving room to capture
solar energy.

#### `enabled` (boolean, default: true)

- Enable/disable this strategy

#### `solar_window_start_hour` (integer, default: 9, range: 0-23)

- Hour when solar production typically begins
- Strategy avoids grid charging shortly before this time

#### `solar_window_end_hour` (integer, default: 12, range: 0-23)

- Hour when peak solar production typically ends
- Defines the morning solar window

#### `midday_max_soc` (float, default: 90.0, range: 0-100)

- Maximum SOC (%) target before solar window
- Leaves headroom to absorb solar production
- Example: `90.0` = keep 10% capacity available for solar

#### `min_solar_forecast_kwh` (float, default: 2.0)

- Minimum solar forecast (kWh) required to activate this strategy
- Prevents unnecessarily limiting charge when little solar is expected
- Lower values = more conservative (activate more often)
- Higher values = only activate when significant solar is forecast

### Seasonal Configuration

#### `force_season` (optional, string)

- Override automatic season detection
- Values:
  - `"winter"` - Force winter strategies
  - `"summer"` - Force summer strategies
  - Not set or empty - Auto-detect season
- Useful for testing or regions with unusual seasons

## Environment Variables

Override configuration values using environment variables (useful for development, testing, or
Docker deployments).

### Available Variables

```bash
# Spot price sensor entity ID
export SPOT_PRICE_ENTITY="sensor.custom_spot_price"

# Debug mode override
export DEBUG_MODE=true    # or false

# Update interval override (seconds)
export UPDATE_INTERVAL_SECS=60

# Home Assistant connection
export HA_BASE_URL="http://homeassistant.local:8123"
export HA_TOKEN="eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
```

### Priority Order

When the same setting is defined in multiple places:

1. **Environment variables** (highest priority)
2. **Configuration file** (config.toml, config.json, or /data/options.json)
3. **Default values** (lowest priority)

### Example Usage

```bash
# Run with debug mode disabled via environment variable
# (even if config.toml has debug_mode = true)
export DEBUG_MODE=false
cargo run --release

# Run with custom HA connection
export HA_BASE_URL="http://192.168.1.100:8123"
export HA_TOKEN="your_token_here"
cargo run --release
```

## Complete Examples

### Example 1: Single Solax Inverter with Spot Prices

Typical Czech household with one inverter and spot price optimization.

```toml
[[inverters]]
id = "solax_main"
vendor = "solax"
entity_prefix = "solax"
topology = "independent"

[pricing]
spot_price_entity = "sensor.current_spot_electricity_price_15min"
use_spot_prices_to_buy = true
use_spot_prices_to_sell = true
fixed_buy_prices = [0.05; 24]
fixed_sell_prices = [0.08; 24]

[control]
maximum_export_power_w = 5000
force_charge_hours = 4
force_discharge_hours = 2
min_battery_soc = 15.0
max_battery_soc = 100.0
hardware_min_battery_soc = 10.0
battery_capacity_kwh = 23.0
battery_wear_cost_czk_per_kwh = 0.125
battery_efficiency = 0.95
min_mode_change_interval_secs = 300
average_household_load_kw = 0.5
min_consecutive_force_blocks = 2

[strategies.winter_peak_discharge]
enabled = true
min_spread_czk = 3.0
min_soc_to_start = 70.0
min_soc_target = 50.0
solar_window_start_hour = 9
solar_window_end_hour = 15
min_hours_to_solar = 4

[strategies.solar_aware_charging]
enabled = true
solar_window_start_hour = 9
solar_window_end_hour = 12
midday_max_soc = 90.0
min_solar_forecast_kwh = 2.0

[system]
debug_mode = true
update_interval_secs = 60
log_level = "info"
display_currency = "CZK"
language = "cs"
```

### Example 2: Multi-Inverter Master/Slave Setup

Three Solax inverters in master-slave configuration for larger installation.

```toml
[[inverters]]
id = "master"
vendor = "solax"
entity_prefix = "solax_master"
topology = "master"
slaves = ["slave_1", "slave_2"]

[[inverters]]
id = "slave_1"
vendor = "solax"
entity_prefix = "solax_s1"
topology = "slave"
master = "master"

[[inverters]]
id = "slave_2"
vendor = "solax"
entity_prefix = "solax_s2"
topology = "slave"
master = "master"

[pricing]
spot_price_entity = "sensor.spot_price"
use_spot_prices_to_buy = true
use_spot_prices_to_sell = true
fixed_buy_prices = [0.05; 24]
fixed_sell_prices = [0.08; 24]

[control]
maximum_export_power_w = 15000    # Higher for multiple inverters
force_charge_hours = 6
force_discharge_hours = 3
min_battery_soc = 15.0
max_battery_soc = 95.0
hardware_min_battery_soc = 10.0
battery_capacity_kwh = 69.0        # 3x 23 kWh batteries
battery_wear_cost_czk_per_kwh = 0.125
battery_efficiency = 0.95
min_mode_change_interval_secs = 300
average_household_load_kw = 1.2    # Larger household
min_consecutive_force_blocks = 2

[strategies.winter_peak_discharge]
enabled = true
min_spread_czk = 2.5
min_soc_to_start = 70.0
min_soc_target = 50.0
solar_window_start_hour = 9
solar_window_end_hour = 15
min_hours_to_solar = 4

[strategies.solar_aware_charging]
enabled = true
solar_window_start_hour = 9
solar_window_end_hour = 12
midday_max_soc = 85.0              # More conservative
min_solar_forecast_kwh = 5.0       # Higher threshold

[system]
debug_mode = false                 # Production mode
update_interval_secs = 60
log_level = "info"
display_currency = "EUR"
language = "en"
```

### Example 3: Fixed Prices (No Spot Market)

For regions without spot prices or fixed-rate contracts.

```toml
[[inverters]]
id = "main"
vendor = "solax"
entity_prefix = "solax"
topology = "independent"

[pricing]
spot_price_entity = "sensor.spot_price"  # Still needed but ignored
use_spot_prices_to_buy = false           # Disable spot prices
use_spot_prices_to_sell = false

# Time-of-use pricing (example for Czech ČEZ D57d tariff)
fixed_buy_prices = [
    # Low rate: 00:00-08:00
    0.04, 0.04, 0.04, 0.04, 0.04, 0.04, 0.04, 0.04,
    # High rate: 08:00-20:00
    0.07, 0.07, 0.07, 0.07, 0.07, 0.07, 0.07, 0.07, 0.07, 0.07, 0.07, 0.07,
    # Low rate: 20:00-00:00
    0.04, 0.04, 0.04, 0.04
]

fixed_sell_prices = [
    # Feed-in tariff (constant throughout day)
    0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05,
    0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05,
    0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05, 0.05
]

[control]
maximum_export_power_w = 5000
force_charge_hours = 8              # Charge during low-rate period
force_discharge_hours = 12          # Discharge during high-rate period
min_battery_soc = 20.0
max_battery_soc = 100.0
hardware_min_battery_soc = 10.0
battery_capacity_kwh = 23.0
battery_wear_cost_czk_per_kwh = 0.125
battery_efficiency = 0.95
min_mode_change_interval_secs = 300
average_household_load_kw = 0.6
min_consecutive_force_blocks = 4    # Longer blocks for TOU

[strategies.winter_peak_discharge]
enabled = false                     # Not useful without price volatility

[strategies.solar_aware_charging]
enabled = true                      # Still useful for solar optimization
solar_window_start_hour = 9
solar_window_end_hour = 12
midday_max_soc = 90.0
min_solar_forecast_kwh = 1.5

[system]
debug_mode = true
update_interval_secs = 60
log_level = "info"
display_currency = "CZK"
language = "cs"
```

## Validation Rules

FluxION validates configuration on startup. Common validation errors:

### Inverter Validation

- At least one inverter must be configured
- Each inverter must have unique `id`
- `entity_prefix` cannot be empty
- Topology must be: `independent`, `master`, or `slave`
- Master inverters must list at least one slave in `slaves`
- Slave inverters must reference a valid master in `master`

### Pricing Validation

- `spot_price_entity` cannot be empty
- `fixed_buy_prices` must have exactly 24 or 96 values
- `fixed_sell_prices` must have exactly 24 or 96 values
- All prices must be non-negative

### Control Validation

- `min_battery_soc` must be between 0 and 100
- `max_battery_soc` must be between 0 and 100
- `min_battery_soc` < `max_battery_soc`
- `min_battery_soc` >= `hardware_min_battery_soc`
- `hardware_min_battery_soc` must be between 0 and 100
- `battery_capacity_kwh` must be positive
- `battery_wear_cost_czk_per_kwh` must be non-negative
- `battery_efficiency` must be between 0.0 and 1.0
- `min_mode_change_interval_secs` must be >= 60 seconds
- `min_consecutive_force_blocks` must be >= 1

### System Validation

- `update_interval_secs` must be >= 10 seconds
- `log_level` must be one of: error, warn, info, debug, trace
- `display_currency` should be one of: EUR, USD, CZK
- `language` should be one of: en, cs

## Troubleshooting

### Configuration File Not Found

**Error**: "No configuration file found, using defaults"

**Solutions**:

1. Create `config.toml` in the working directory
2. Copy from `config.example.toml`
3. Or set configuration via environment variables

### Failed to Parse Configuration

**Error**: "Failed to parse config.toml: ..."

**Solutions**:

1. Check TOML syntax (use a TOML validator)
2. Ensure all required fields are present
3. Check for missing commas in arrays
4. Ensure quotes around strings
5. Validate number formats (no trailing commas)

### Inverter Configuration Errors

**Error**: "Configuration must include at least one inverter"

- Add at least one `[[inverters]]` section

**Error**: "Inverter 'X' is configured as master but has no slaves"

- Add `slaves = ["slave_id"]` to master configuration

**Error**: "Inverter 'X' is configured as slave but has no master"

- Add `master = "master_id"` to slave configuration

### Price Configuration Errors

**Error**: "fixed_buy_prices must have 24 or 96 values, got X"

- Ensure price arrays have exactly 24 (hourly) or 96 (15-min) values
- Count your array elements carefully
- Each hour needs one value for 24-hour format

### Battery SOC Errors

**Error**: "min_battery_soc must be less than max_battery_soc"

- Check that `min_battery_soc` < `max_battery_soc`
- Example: min=15.0, max=100.0 (correct)
- Example: min=80.0, max=70.0 (incorrect)

**Error**: "min_battery_soc must be between 0 and 100"

- Ensure SOC values are percentages (0-100)
- Don't use decimal notation like 0.15 for 15%

### Home Assistant Connection Errors

**Error**: "Failed to connect to Home Assistant"

**Solutions**:

1. Verify `ha_base_url` is correct and accessible
2. Check that `ha_token` is valid and not expired
3. Ensure Home Assistant is running
4. Verify network connectivity
5. Check firewall settings
6. For addon mode: ensure running in HA addon environment

### Strategy Configuration Errors

**Warning**: "force_charge_hours is 0 - no charging will be scheduled"

- This is intentional if you only want solar charging
- Set to non-zero value for grid charging

**Warning**: "force_discharge_hours is 0 - no discharging will be scheduled"

- This is intentional if you only want self-consumption
- Set to non-zero value for arbitrage discharge

## Getting Help

### Home Assistant Long-Lived Access Token

For development outside HA addon:

1. Log into Home Assistant web interface
2. Click your profile icon (bottom left)
3. Scroll down to "Long-Lived Access Tokens" section
4. Click "Create Token"
5. Give it a descriptive name (e.g., "FluxION Development")
6. Copy the token immediately (shown only once)
7. Add to `config.toml`:
   ```toml
   [system]
   ha_token = "YOUR_TOKEN_HERE"
   ```

**Security Warning**: Keep tokens secret! Add `config.toml` to `.gitignore` if it contains tokens.

### Checking Configuration

Use the validation built into FluxION:

```bash
# Run FluxION - it will validate config on startup
cargo run --release

# Check logs for validation errors
# Look for lines like:
# ✅ Loaded configuration from config.toml
# ❌ Configuration validation failed: ...
```

### Support Resources

- **Documentation**: See `/fluxion/docs/` directory
- **Configuration Guide**: This file
- **Deployment Guide**: `docs/guides/DEPLOYMENT.md`
- **Architecture**: `docs/architecture/ARCHITECTURE.md`
- **Issue Tracker**: https://github.com/SolarE-cz/fluxion/issues
- **Commercial Support**: info@solare.cz

______________________________________________________________________

**Last Updated**: 2025-10-31 **FluxION Version**: 0.1.0 (MVP)
