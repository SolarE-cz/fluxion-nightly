# FluxION Integration Tests

This crate contains integration tests that connect to a real Home Assistant instance.

## Setup

### 1. Get Home Assistant Token

1. Log into your Home Assistant instance
2. Go to your Profile (click your name in the bottom left)
3. Scroll down to **Long-Lived Access Tokens**
4. Click **Create Token**
5. Give it a name like "FluxION Tests"
6. Copy the generated token

### 2. Save Token

Create `.token.txt` in the **workspace root** (not in this directory):

```bash
cd /home/daniel/Repositories/solare/main/fluxion
echo "YOUR_TOKEN_HERE" > .token.txt
```

**Important**: The token must have full access to read/write Home Assistant entities.

### 3. Verify HA URL

The tests default to `http://homeassistant.local:8123`. If your HA is at a different address, update
the `base_url` in the test files.

## Running Tests

### Run All Integration Tests

```bash
./test-integration.sh
```

Or manually:

```bash
cargo test --package fluxion-integration-tests --test ha_integration -- --ignored --nocapture
```

### Run Specific Test

```bash
./test-integration.sh test_ha_connection
```

Or manually:

```bash
cargo test --package fluxion-integration-tests --test ha_integration test_ha_connection -- --ignored --nocapture
```

## Available Tests

1. **test_ha_connection** - Basic connectivity (doesn't require auth)
2. **test_read_single_entity** - Read sun.sun entity
3. **test_get_all_entities** - Discover all entities in HA
4. **test_solax_battery_reading** - Read Solax battery SOC
5. **test_solax_inverter_adapter** - Test full inverter adapter
6. **test_cz_spot_price_adapter** - Test Czech spot price reading
7. **test_end_to_end_data_flow** - Complete pipeline test

## Troubleshooting

### "AuthenticationFailed" Error

**Cause**: Token is invalid, expired, or has insufficient permissions.

**Solution**:

1. Generate a new Long-Lived Access Token in HA
2. Make sure it's a **Long-Lived** token, not a temporary one
3. Update `.token.txt` with the new token
4. The token should look like: `eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9...` (very long string)

### "No such file or directory" (.token.txt)

**Cause**: Token file not found in workspace root.

**Solution**:

```bash
cd /home/daniel/Repositories/solare/main/fluxion
echo "YOUR_TOKEN" > .token.txt
```

### "Connection refused"

**Cause**: Cannot connect to Home Assistant.

**Solution**:

1. Check if HA is running
2. Verify the URL (default: `http://homeassistant.local:8123`)
3. Update `base_url` in tests if needed
4. Check network connectivity: `curl http://homeassistant.local:8123/api/`

### "No battery entity found"

**Cause**: Solax integration not configured or entities have different names.

**Solution**:

1. Verify Solax integration is installed and working in HA
2. Run `test_get_all_entities` to see actual entity names
3. Update entity search logic if names differ

## CI/CD Integration

These tests are marked with `#[ignore]` so they don't run in regular CI pipelines.

To run them in CI, you would need to:

1. Set up a test Home Assistant instance
2. Provide the token as a CI secret
3. Ensure network connectivity from CI runners
4. Run with: `cargo test -- --ignored`

For now, these tests are manual only.
