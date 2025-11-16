# FluxION ECS - Deployment Guide

## Quick Start - Integration Testing

### 1. Save HA Token

```bash
cd /home/daniel/Repositories/solare/fluxion

# Save your token (replace with actual token)
echo "YOUR_HA_TOKEN_HERE" > .token.txt
```

### 2. Run Entity Discovery

This will show all entities in your HA instance:

```bash
cargo test --test integration_ha test_get_all_entities -- --ignored --nocapture
```

Look for:

- Solax inverter entities (battery_capacity, work_mode, grid_power, etc.)
- Spot price entity (for Czech energy prices)

### 3. Run Basic Connectivity Test

```bash
cargo test --test integration_ha test_ha_connection -- --ignored --nocapture
```

### 4. Test Inverter Reading

```bash
cargo test --test integration_ha test_solax_inverter_adapter -- --ignored --nocapture
```

This will show current:

- Battery SOC
- Work mode
- Grid/Battery/PV power

### 5. Test End-to-End Flow

```bash
cargo test --test integration_ha test_end_to_end_data_flow -- --ignored --nocapture
```

This tests the complete pipeline:

1. Read inverter state
2. Read spot prices
3. Analyze prices
4. Generate schedule

______________________________________________________________________

## CI/CD Pipeline Setup

### GitLab CI/CD

The `.gitlab-ci.yml` file is already configured. To enable:

1. **Push to GitLab**:

   ```bash
   git remote add gitlab https://gitlab.com/your-org/fluxion.git
   git push gitlab dhk/fluxion/mvp
   ```

2. **Configure GitLab Variables** (Settings → CI/CD → Variables):

   - `CI_REGISTRY_USER` - Your GitLab username
   - `CI_REGISTRY_PASSWORD` - Your GitLab access token
   - `CI_REGISTRY` - GitLab container registry URL

3. **Pipeline Stages**:

   - **Test**: Runs on every commit

     - Unit tests
     - Clippy checks
     - Format checks
     - Security audit

   - **Build**: Runs on `main` and tags

     - Builds Docker images for amd64, aarch64, armv7
     - Pushes to container registry

   - **Release**: Runs on tags only

     - Creates multi-arch manifest
     - Generates release notes

### Creating a Release

```bash
# Tag a release
git tag -a v0.1.0 -m "FluxION MVP Release"
git push --tags

# GitLab CI will automatically:
# 1. Run all tests
# 2. Build multi-arch Docker images
# 3. Create release with artifacts
```

______________________________________________________________________

## Home Assistant Addon Deployment

### Method 1: Local Development

For testing without Docker:

```bash
# Build release binary
cargo build --release

# Run with your HA config
./target/release/fluxion-main
```

Configuration will be loaded from:

1. `/data/options.json` (HA addon mode)
2. `config.toml` (local file)
3. `config.json` (local file)
4. Environment variables

### Method 2: Docker Build Locally

```bash
# Build for your architecture
docker build -t fluxion:local .

# Run with HA supervisor
docker run --rm \
  -e SUPERVISOR_TOKEN="your_token" \
  -v /path/to/config:/data \
  fluxion:local
```

### Method 3: Install as HA Addon

1. **Create Repository**:

   Create a new repository with this structure:

   ```
   fluxion-addon/
   ├── fluxion/
   │   ├── config.yaml
   │   ├── Dockerfile
   │   ├── run.sh
   │   └── ... (all source files)
   └── repository.yaml
   ```

2. **repository.yaml**:

   ```yaml
   name: FluxION ECS Add-ons
   url: https://gitlab.com/your-org/fluxion-addon
   maintainer: Your Name
   ```

3. **Add to Home Assistant**:

   - Settings → Add-ons → Add-on Store → ⋮ → Repositories
   - Add URL: `https://gitlab.com/your-org/fluxion-addon`
   - Install "FluxION ECS"

4. **Configure**:

   Edit addon configuration in HA UI or via YAML:

   ```yaml
   debug_mode: true  # Start in safe mode
   log_level: info
   inverters:
     - id: solax
       vendor: solax
       entity_prefix: solax
       topology: independent
       min_battery_soc: 10
       max_battery_soc: 100
   pricing:
     spot_price_entity: sensor.current_spot_electricity_prices
     use_spot_prices_to_buy: true
     use_spot_prices_to_sell: true
     force_charge_hours: 4
     force_discharge_hours: 2
   control:
     maximum_export_power_w: 10000
     update_interval_secs: 60
   ```

5. **Start Addon**:

   - Start the addon
   - Check logs for status
   - In debug mode, it will log actions but not execute them

6. **Enable Production Mode**:

   - Once tested, set `debug_mode: false`
   - Restart addon
   - System will now make real changes to inverter

______________________________________________________________________

## Configuration Reference

### Inverter Configuration

```yaml
inverters:
  - id: "unique_id"              # Unique identifier
    vendor: "solax"               # Vendor: solax, fronius, sma
    entity_prefix: "solax"        # HA entity prefix
    topology: "independent"       # independent, master, or slave
    master_id: "master_id"        # If slave, ID of master inverter
    slave_ids: ["slave1"]         # If master, IDs of slave inverters
    min_battery_soc: 10           # Minimum SOC for discharge (%)
    max_battery_soc: 100          # Maximum SOC for charge (%)
```

### Pricing Configuration

```yaml
pricing:
  spot_price_entity: "sensor.current_spot_electricity_prices"  # HA spot price entity
  use_spot_prices_to_buy: true                              # Use spot prices for charge decisions
  use_spot_prices_to_sell: true                             # Use spot prices for discharge decisions
  force_charge_hours: 4                              # Hours to charge (cheapest)
  force_discharge_hours: 2                           # Hours to discharge (most expensive)
  fixed_buy_prices: [0.05, 0.05, ...]              # 24 hourly prices (fallback)
  fixed_sell_prices: [0.08, 0.08, ...]             # 24 hourly prices (fallback)
```

### Control Configuration

```yaml
control:
  maximum_export_power_w: 10000      # Maximum export power limit
  update_interval_secs: 60       # How often to check schedule (seconds)
```

______________________________________________________________________

## Monitoring & Debugging

### Check Addon Logs

In Home Assistant:

- Settings → Add-ons → FluxION ECS → Log

Or via command line:

```bash
ha addons logs fluxion
```

### Debug Mode

Always start in debug mode to verify configuration:

```yaml
debug_mode: true
```

In debug mode:

- ✅ All logic runs normally
- ✅ Schedules are generated
- ✅ Mode changes are determined
- 🔍 Actions are LOGGED but NOT executed
- 🔍 Log shows "Would change mode to ForceCharge" instead of actually changing

### Production Mode

Once verified in debug mode:

```yaml
debug_mode: false
```

In production mode:

- ⚠️ System makes REAL changes to inverter
- ⚠️ Modes are actually switched
- ⚠️ Battery is charged/discharged
- ⚠️ Export limits are set

### Log Levels

```yaml
log_level: debug  # trace, debug, info, warn, error
```

- `trace`: Everything (very verbose)
- `debug`: Detailed debugging info
- `info`: Normal operation (recommended)
- `warn`: Warnings and errors only
- `error`: Errors only

______________________________________________________________________

## Troubleshooting

### "Failed to read entity"

**Symptom**: Error reading Solax entities

**Solution**:

1. Check entity names in HA (Developer Tools → States)
2. Verify entity_prefix matches your Solax integration
3. Run discovery test to see available entities:
   ```bash
   cargo test --test integration_ha test_get_all_entities -- --ignored --nocapture
   ```

### "No spot price entity found"

**Symptom**: Can't read price data

**Solution**:

1. Install Czech Energy Spot Prices integration in HA
2. Check entity name: `sensor.current_spot_electricity_prices`
3. Update `spot_price_entity` in configuration
4. Or use `fixed_buy_prices` / `fixed_sell_prices` as fallback

### "Inverter not responding"

**Symptom**: Can't control inverter

**Solution**:

1. Verify Solax integration is working in HA
2. Check work mode entity: `select.solax_charger_use_mode`
3. Test manually changing mode in HA
4. Ensure addon has `homeassistant_api: true` in config.yaml

### "Schedule not executing"

**Symptom**: Schedule generated but modes don't change

**Solution**:

1. Check if `debug_mode: true` (won't execute if in debug mode)
2. Verify `update_interval_secs` isn't too long
3. Check SOC constraints (min/max battery SOC)
4. Look for "Minimum interval" messages (prevents rapid switching)

______________________________________________________________________

## Performance Tuning

### Update Interval

```yaml
update_interval_secs: 60  # Default: every 60 seconds
```

- Lower = More responsive, more CPU/network
- Higher = Less responsive, less resource usage
- Recommended: 60-120 seconds

### Price Update Frequency

Price data is checked every update cycle, but schedule is only regenerated when prices change.

### Minimum Mode Change Interval

Configured in `ExecutionConfig` (60 seconds default). Prevents rapid switching that could damage
inverter.

______________________________________________________________________

## Safety Features

1. **Debug Mode Default**: Always starts in safe mode
2. **Minimum Interval**: Prevents rapid mode switching (60s)
3. **SOC Constraints**: Respects min/max battery SOC
4. **Connection Health**: Monitors HA connectivity
5. **Graceful Degradation**: Falls back to fixed prices if spot prices unavailable

______________________________________________________________________

## Next Steps

1. ✅ **Test Integration**: Run all integration tests with your HA
2. ✅ **Verify Entity Discovery**: Confirm all Solax entities found
3. ✅ **Test in Debug Mode**: Deploy addon with `debug_mode: true`
4. ✅ **Monitor Logs**: Watch for 24 hours in debug mode
5. ✅ **Enable Production**: Set `debug_mode: false` when ready
6. 🚧 **Complete Phase 5**: Implement continuous monitoring systems
7. 🚧 **Complete Phase 6**: Wire all plugins together

______________________________________________________________________

## Support & Documentation

- **Architecture**: [ARCHITECTURE.md](ARCHITECTURE.md)
- **Testing Guide**: [TESTING.md](TESTING.md)
- **Implementation Status**: [MVP_STATUS.md](MVP_STATUS.md)
- **Requirements**: [REQUIREMENTS_QUICK_REF.md](REQUIREMENTS_QUICK_REF.md)
- **Implementation Plan**: [IMPLEMENTATION_PLAN.md](IMPLEMENTATION_PLAN.md)

______________________________________________________________________

**Status**: Ready for real-world integration testing and CI/CD pipeline setup!
