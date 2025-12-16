# FluxION Testing Guide

## Unit Tests

Run all unit tests (no external dependencies):

```bash
cargo test --workspace
```

This runs 71 unit tests covering:

- fluxion-core: 56 tests
- fluxion-ha: 9 tests
- fluxion-solax: 6 tests

## Integration Tests

Integration tests connect to a real Home Assistant instance.

### Prerequisites

1. **Home Assistant instance** running and accessible
2. **Long-lived access token** from HA:
   - Go to your HA profile
   - Scroll to "Long-Lived Access Tokens"
   - Create a new token
   - Save the token to `.token.txt` in the project root

```bash
echo "your_token_here" > .token.txt
```

**Note:** `.token.txt` is in `.gitignore` and will not be committed.

### Running Integration Tests

Integration tests are marked with `#[ignore]` and must be explicitly run:

```bash
# Run all integration tests
cargo test --test integration_ha -- --ignored --nocapture

# Run specific integration test
cargo test --test integration_ha test_ha_connection -- --ignored --nocapture

# Discovery: List all entities in your HA instance
cargo test --test integration_ha test_get_all_entities -- --ignored --nocapture
```

### Available Integration Tests

1. **test_ha_connection** - Basic connectivity test

   ```bash
   cargo test --test integration_ha test_ha_connection -- --ignored --nocapture
   ```

2. **test_get_all_entities** - Discovery tool to see all entities

   ```bash
   cargo test --test integration_ha test_get_all_entities -- --ignored --nocapture
   ```

   This shows:

   - Total entity count
   - All Solax entities
   - All price/energy entities

3. **test_solax_battery_reading** - Read battery SOC

   ```bash
   cargo test --test integration_ha test_solax_battery_reading -- --ignored --nocapture
   ```

4. **test_solax_inverter_adapter** - Full inverter state reading

   ```bash
   cargo test --test integration_ha test_solax_inverter_adapter -- --ignored --nocapture
   ```

   Reads:

   - Battery SOC
   - Work mode
   - Grid power
   - Battery power
   - PV power

5. **test_cz_spot_price_adapter** - Read Czech spot prices

   ```bash
   cargo test --test integration_ha test_cz_spot_price_adapter -- --ignored --nocapture
   ```

6. **test_end_to_end_data_flow** - Complete workflow test

   ```bash
   cargo test --test integration_ha test_end_to_end_data_flow -- --ignored --nocapture
   ```

   Tests entire pipeline:

   - Read inverter state
   - Read spot prices
   - Analyze prices
   - Generate schedule

### Troubleshooting

**Error: "Failed to read .token.txt"**

- Create the `.token.txt` file with your HA token

**Error: "No battery entity found"**

- Run `test_get_all_entities` to see available entities
- Your Solax integration might use different entity names
- Update the entity search logic in tests if needed

**Error: "Failed to ping HA"**

- Check HA URL in test (default: `http://homeassistant.local:8123`)
- Update URL if your HA is at a different address
- Check network connectivity

**Error: "No spot price entity found"**

- You need a price integration (e.g., Czech Spot Prices)
- Run `test_get_all_entities` to verify price entities exist
- Price tests will be skipped if no price entity found

## CI/CD Pipeline

The GitLab CI pipeline runs:

- Unit tests on every commit
- Clippy checks
- Release builds

Integration tests are NOT run in CI (they require a real HA instance).

## Code Coverage

To generate code coverage report:

```bash
# Install tarpaulin
cargo install cargo-tarpaulin

# Generate coverage
cargo tarpaulin --workspace --out Html --output-dir coverage
```

Open `coverage/index.html` in your browser.

## Performance Testing

Run benchmarks (when available):

```bash
cargo bench
```

## Debugging Tests

Run tests with debug output:

```bash
RUST_LOG=debug cargo test --test integration_ha test_ha_connection -- --ignored --nocapture
```

Log levels:

- `error` - Errors only
- `warn` - Warnings and errors
- `info` - Info, warnings, and errors (default)
- `debug` - Verbose debugging
- `trace` - Very verbose (all details)

## Writing New Tests

### Unit Tests

Add to module files:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    
    #[test]
    fn test_something() {
        // Test code
    }
}
```

### Integration Tests

Add to `tests/integration_ha.rs`:

```rust
#[tokio::test]
#[ignore]
async fn test_new_feature() {
    let token = load_token().expect("Failed to read .token.txt");
    // Test code
}
```

Remember to mark as `#[ignore]` so it doesn't run in regular test suite.
