# FluxION ECS Configuration Guide

This guide explains all configuration options for the FluxION ECS (Energy Control System).

## Configuration Files

FluxION ECS supports multiple configuration sources in order of precedence:

1. **`/data/options.json`** - Home Assistant addon options (JSON format)
2. **`config.toml`** - Local TOML configuration file (preferred for development)
3. **`config.json`** - Local JSON configuration file
4. **Environment variables** - Fallback with default values

## Quick Start

### For Development

1. Copy the example configuration:

   ```bash
   cp config.example.toml config.toml
   ```

2. Edit `config.toml` with your settings

3. Set your Home Assistant token (if running outside HA addon):

   ```toml
   [system]
   ha_base_url = "http://homeassistant.local:8123"
   ha_token = "your_long_lived_access_token"
   ```

4. Run the application:

   ```bash
   cargo run
   ```

### For Home Assistant Addon

Configuration is handled through the addon UI. The addon will use `/data/options.json`
automatically.

## Configuration Sections

### 1. Inverters (`[[inverters]]`)

Configure one or more inverters. At least one inverter is required.

```toml
[[inverters]]
id = "main_inverter"           # Unique identifier
vendor = "solax"                # Vendor: solax, fronius, sma
entity_prefix = "solax"         # HA entity prefix (e.g., sensor.solax_battery_soc)
topology = "independent"        # Topology: independent, master, slave
```

#### Topology Options

**Independent** - Single inverter or multiple independent inverters:

```toml
[[inverters]]
id = "inverter_1"
topology = "independent"
```

**Master/Slave** - Multiple inverters with one controlling the others:

```toml
[[inverters]]
id = "master_inv"
topology = "master"
slaves = ["slave_1", "slave_2"]

[[inverters]]
id = "slave_1"
topology = "slave"
master = "master_inv"
```

### 2. Pricing (`[pricing]`)

Configure electricity pricing for optimization decisions.

```toml
[pricing]
# Home Assistant sensor providing spot prices
spot_price_entity = "sensor.current_spot_electricity_price_15min"

# Use spot prices for buy/sell decisions
use_spot_prices_to_buy = true
use_spot_prices_to_sell = true

# Fallback fixed prices (24 hourly values)
# Used when spot prices are disabled or unavailable
fixed_buy_prices = [
    0.05, 0.05, 0.05, 0.05, 0.05, 0.05,  # Night (00:00-05:59)
    0.06, 0.07, 0.08, 0.08, 0.07, 0.06,  # Morning (06:00-11:59)
    0.06, 0.07, 0.08, 0.08, 0.09, 0.10,  # Afternoon (12:00-17:59)
    0.09, 0.08, 0.07, 0.06, 0.05, 0.05   # Evening (18:00-23:59)
]

fixed_sell_prices = [
    0.08, 0.08, 0.08, 0.08, 0.08, 0.08,
    0.09, 0.10, 0.11, 0.11, 0.10, 0.09,
    0.09, 0.10, 0.11, 0.11, 0.12, 0.13,
    0.12, 0.11, 0.10, 0.09, 0.08, 0.08
]
```

**Notes:**

- Prices are in your local currency per kWh (e.g., CZK/kWh, EUR/kWh)
- Fixed prices must have exactly 24 values (one per hour) or 96 values (one per 15-min block)
- If spot prices are disabled, fixed prices are used for all decisions

### 3. Control (`[control]`)

Configure battery and export power control parameters.

```toml
[control]
maximum_export_power_w = 5000    # Max grid export power (watts)
force_charge_hours = 4           # Charge during N cheapest hours
force_discharge_hours = 2        # Discharge during N most expensive hours
min_battery_soc = 10.0          # Minimum battery SoC (%)
max_battery_soc = 100.0         # Maximum battery SoC (%)
```

**Parameters:**

- **`maximum_export_power_w`** - Maximum power to export to grid (in watts)

  - Typical values: 3000-10000W depending on grid connection

- **`force_charge_hours`** - How many of the cheapest hours to force battery charging

  - Set to 0 to disable forced charging
  - Example: 4 means charge during the 4 cheapest price periods

- **`force_discharge_hours`** - How many of the most expensive hours to force battery discharge

  - Set to 0 to disable forced discharging
  - Example: 2 means discharge during the 2 most expensive price periods

- **`min_battery_soc`** - Minimum battery state of charge (0-100%)

  - Safety limit to prevent deep discharge
  - Typical value: 10-20%

- **`max_battery_soc`** - Maximum battery state of charge (0-100%)

  - Upper limit for charging
  - Typical value: 90-100%

### 4. System (`[system]`)

System-wide configuration settings.

```toml
[system]
debug_mode = true               # Enable debug mode (safe mode)
update_interval_secs = 60       # Update interval in seconds
log_level = "info"              # Logging level

# Optional: Home Assistant connection
ha_base_url = "http://homeassistant.local:8123"  # HA URL
ha_token = "your_token_here"                      # HA long-lived access token
```

**Parameters:**

- **`debug_mode`** (boolean)

  - `true`: Logs all actions but doesn't make actual hardware changes (safe for testing)
  - `false`: Makes real changes to inverter settings
  - **Default: `true`** for safety

- **`update_interval_secs`** (integer)

  - How often to run the control loop (in seconds)
  - Minimum: 10 seconds
  - Recommended: 60 seconds
  - Higher values reduce system load but decrease responsiveness

- **`log_level`** (string)

  - Options: `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`
  - Default: `"info"`
  - Use `"debug"` for troubleshooting

- **`ha_base_url`** (optional string)

  - Home Assistant base URL
  - Required for development/testing outside HA addon
  - Leave unset when running as HA addon (uses supervisor automatically)

- **`ha_token`** (optional string)

  - Home Assistant long-lived access token
  - Required for development/testing outside HA addon
  - Leave unset when running as HA addon (uses `SUPERVISOR_TOKEN` env var)

## Environment Variable Overrides

You can override configuration values using environment variables:

```bash
# Override debug mode
export DEBUG_MODE=false

# Override update interval
export UPDATE_INTERVAL_SECS=30

# Override HA connection
export HA_BASE_URL="http://homeassistant.local:8123"
export HA_TOKEN="your_token_here"

# Override spot price entity
export SPOT_PRICE_ENTITY="sensor.custom_spot_price"

# Run with overrides
cargo run
```

## Configuration Priority

When multiple sources provide the same setting, they are applied in this order:

1. **Environment variables** (highest priority)
2. **Configuration file** (`config.toml`, `config.json`, or `/data/options.json`)
3. **Default values** (lowest priority)

## Getting a Home Assistant Long-Lived Access Token

For development outside the HA addon:

1. Log into Home Assistant
2. Click your profile (bottom left)
3. Scroll down to "Long-Lived Access Tokens"
4. Click "Create Token"
5. Give it a name (e.g., "FluxION ECS Dev")
6. Copy the token and add it to your `config.toml`

⚠️ **Security Warning**: Keep your token secret! Don't commit it to git. Consider using:

- A separate `config.local.toml` (add to `.gitignore`)
- Environment variables instead of config files
- Secret management tools for production

## Validation

The application validates all configuration on startup:

- At least one inverter must be configured
- All required fields must be present
- Values must be within valid ranges (e.g., SoC 0-100%)
- Topology relationships must be consistent (master→slaves, slave→master)
- Fixed prices must have 24 or 96 values
- Update interval must be at least 10 seconds

If validation fails, the application will exit with a clear error message explaining what needs to
be fixed.

## Example Configurations

### Single Inverter Setup

```toml
[[inverters]]
id = "solax_main"
vendor = "solax"
entity_prefix = "solax"
topology = "independent"

[pricing]
spot_price_entity = "sensor.nordpool_spot_price"
use_spot_prices_to_buy = true
use_spot_prices_to_sell = true
fixed_buy_prices = [0.05; 24]
fixed_sell_prices = [0.08; 24]

[control]
maximum_export_power_w = 5000
force_charge_hours = 4
force_discharge_hours = 2
min_battery_soc = 10.0
max_battery_soc = 100.0

[system]
debug_mode = true
update_interval_secs = 60
log_level = "info"
```

### Multi-Inverter Master/Slave Setup

```toml
[[inverters]]
id = "master"
vendor = "solax"
entity_prefix = "solax_1"
topology = "master"
slaves = ["slave_1", "slave_2"]

[[inverters]]
id = "slave_1"
vendor = "solax"
entity_prefix = "solax_2"
topology = "slave"
master = "master"

[[inverters]]
id = "slave_2"
vendor = "solax"
entity_prefix = "solax_3"
topology = "slave"
master = "master"

[pricing]
spot_price_entity = "sensor.spot_price"
use_spot_prices_to_buy = true
use_spot_prices_to_sell = true
fixed_buy_prices = [0.05; 24]
fixed_sell_prices = [0.08; 24]

[control]
maximum_export_power_w = 15000  # Higher for multiple inverters
force_charge_hours = 6
force_discharge_hours = 3
min_battery_soc = 15.0
max_battery_soc = 95.0

[system]
debug_mode = false  # Production mode
update_interval_secs = 60
log_level = "info"
```

## Troubleshooting

### "Failed to parse config.toml"

- Check TOML syntax (use a TOML validator)
- Ensure all required sections are present
- Check for missing commas in arrays

### "missing field `inverters`"

- Ensure you have at least one `[[inverters]]` section
- Check spelling of section names

### "Configuration must include at least one inverter"

- Add at least one `[[inverters]]` section to your config

### "min_battery_soc must be less than max_battery_soc"

- Ensure `min_battery_soc < max_battery_soc`
- Both values must be between 0 and 100

### "Failed to connect to Home Assistant"

- Check `ha_base_url` is correct
- Verify `ha_token` is valid and not expired
- Ensure Home Assistant is accessible from your network
- Check firewall settings

## See Also

- [Architecture Documentation](ARCHITECTURE.md)
- [Implementation Plan](IMPLEMENTATION_PLAN.md)
- [Home Assistant Integration](addon/README.md)
