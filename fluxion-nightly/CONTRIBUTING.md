# Contributing to FluxION

Thank you for your interest in contributing to FluxION! This document provides guidelines and
information for contributors.

## Table of Contents

- [Code of Conduct](#code-of-conduct)
- [Getting Started](#getting-started)
- [Development Setup](#development-setup)
- [How to Contribute](#how-to-contribute)
- [Code Style Guidelines](#code-style-guidelines)
- [Testing Requirements](#testing-requirements)
- [Submitting Changes](#submitting-changes)
- [Architecture Guidelines](#architecture-guidelines)

## Code of Conduct

Be respectful and constructive in all interactions. We're building an open-source community focused
on solar energy optimization.

## Getting Started

### Prerequisites

- **Rust:** 1.75 or later
- **Git:** For version control
- **Home Assistant instance** (for testing integrations)
- Basic understanding of:
  - Rust programming
  - Entity Component System (ECS) architecture
  - Solar inverter systems (helpful but not required)

### Development Setup

1. **Clone the repository:**

```bash
git clone https://github.com/yourusername/fluxion.git
cd fluxion
```

2. **Build the project:**

```bash
cargo build
```

3. **Run tests:**

```bash
cargo test --all
```

4. **Set up configuration:**

```bash
cp config.example.toml config.toml
# Edit config.toml with your settings
```

5. **Run the application:**

```bash
cargo run
```

## How to Contribute

### Types of Contributions

We welcome:

- **Bug reports** - File an issue with reproduction steps
- **Feature requests** - Describe the use case and expected behavior
- **Code contributions** - Bug fixes, new features, performance improvements
- **Documentation** - Improvements to guides, examples, or API docs
- **Translations** - Add or improve language support (see [I18N.md](docs/guides/I18N.md))
- **Testing** - Add test cases, improve test coverage

### Finding Issues to Work On

- Check the [issue tracker](https://github.com/yourusername/fluxion/issues)
- Look for issues labeled `good first issue` or `help wanted`
- Ask in discussions if you need guidance

## Code Style Guidelines

### Rust Code Style

FluxION follows standard Rust conventions:

1. **Run rustfmt before committing:**

```bash
cargo fmt --all
```

2. **Run clippy and fix warnings:**

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

3. **Naming conventions:**

   - Use `snake_case` for functions, variables, modules
   - Use `PascalCase` for types, structs, enums
   - Use `SCREAMING_SNAKE_CASE` for constants

4. **Documentation:**

   - Add doc comments (`///`) for public functions
   - Include examples in doc comments where helpful
   - Document panic conditions and safety requirements

### ECS Architecture Guidelines

FluxION uses the Bevy ECS framework. Follow these principles:

1. **Separation of Concerns:**

   - **Components** = Data only (no behavior)
   - **Systems** = Logic only (operate on components)
   - **Resources** = Global shared state

2. **Component Design:**

   - Keep components small and focused
   - Use `Option<T>` for optional data
   - Avoid nesting components

3. **System Design:**

   - Systems should be pure functions where possible
   - Use queries to access component data
   - Communicate between systems via components or events

4. **Example:**

```rust
// Good: Component with data only
#[derive(Component)]
pub struct BatteryStatus {
    pub soc_percent: u16,
    pub power_w: i32,
}

// Good: System operating on components
pub fn battery_monitor_system(
    query: Query<(&Inverter, &BatteryStatus)>
) {
    for (inverter, battery) in query.iter() {
        // Logic here
    }
}
```

### Vendor-Agnostic Design

When adding inverter support:

1. **Use the `VendorEntityMapper` trait** for brand-specific logic
2. **Add data to `GenericInverterState`** for new sensor types
3. **Keep business logic vendor-independent** in the core module
4. **Example:** See `crates/fluxion-solax/src/entity_mapper.rs`

## Testing Requirements

### Running Tests

```bash
# Run all tests
cargo test --all

# Run tests for specific crate
cargo test -p fluxion-core

# Run with output
cargo test --all -- --nocapture
```

### Test Coverage Requirements

- **New features:** Must include unit tests
- **Bug fixes:** Add regression test demonstrating the fix
- **Integration tests:** Use for testing system interactions

### Writing Tests

1. **Unit tests:** Test individual functions/components

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_battery_soc_prediction() {
        // Test implementation
    }
}
```

2. **Integration tests:** Test component interactions

```rust
// In crates/fluxion-integration-tests/
#[test]
fn test_schedule_generation() {
    // Test full schedule generation pipeline
}
```

### Test Quality

- Use descriptive test names (`test_charge_block_consolidation_with_gaps`)
- Test edge cases (zero values, max values, boundary conditions)
- Use assertions with clear messages
- Avoid testing implementation details - test behavior

## Submitting Changes

### Before Submitting a Pull Request

1. **Ensure tests pass:**

```bash
cargo test --all
```

2. **Check code formatting:**

```bash
cargo fmt --all -- --check
```

3. **Fix clippy warnings:**

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

4. **Update documentation:**
   - Update README.md if adding features
   - Add/update doc comments
   - Update relevant guides in `docs/`

### Pull Request Process

1. **Create a feature branch:**

```bash
git checkout -b feature/your-feature-name
```

2. **Make your changes:**

   - Write clear, focused commits
   - Follow commit message conventions (see below)

3. **Push to your fork:**

```bash
git push origin feature/your-feature-name
```

4. **Open a pull request:**
   - Provide clear description of changes
   - Reference related issues
   - Include screenshots for UI changes

### Commit Message Format

Use conventional commit format:

```
type(scope): Short description

Longer description if needed.

Fixes #123
```

**Types:**

- `feat:` - New feature
- `fix:` - Bug fix
- `docs:` - Documentation changes
- `test:` - Test additions/changes
- `refactor:` - Code restructuring without behavior change
- `perf:` - Performance improvements
- `chore:` - Build/tooling changes

**Examples:**

```
feat(scheduler): Add support for battery wear cost optimization

Implements economic optimization that accounts for battery degradation
costs when deciding charge/discharge schedules.

Fixes #45
```

```
fix(ha-client): Handle connection timeout gracefully

Previously the application would panic on HA connection timeout.
Now it retries with exponential backoff.

Fixes #78
```

## Architecture Guidelines

### Project Structure

```
fluxion/
├── crates/
│   ├── fluxion-core/      # Core ECS logic, scheduling, strategies
│   ├── fluxion-ha/        # Home Assistant integration
│   ├── fluxion-solax/     # Solax vendor implementation
│   ├── fluxion-web/       # Web UI and API
│   └── fluxion-main/      # Main binary
├── docs/                  # Documentation
└── config.example.toml    # Example configuration
```

### Module Organization

- Keep related functionality together
- Use `mod.rs` for module public API
- Split large files (>1000 lines) into submodules
- See [ARCHITECTURE.md](docs/architecture/ARCHITECTURE.md) for details

### Adding a New Inverter Brand

1. Create new crate: `crates/fluxion-{vendor}/`
2. Implement `VendorEntityMapper` trait
3. Define vendor-specific modes/entities
4. Add integration tests
5. Update documentation

See `crates/fluxion-solax/` as reference implementation.

### Adding a New Strategy

1. Create strategy in `crates/fluxion-core/src/strategy/`
2. Implement evaluation logic
3. Add configuration options
4. Add unit tests
5. Document in architecture guide

## Questions?

- Check the [documentation](docs/README.md)
- Open a [discussion](https://github.com/yourusername/fluxion/discussions)
- File an [issue](https://github.com/yourusername/fluxion/issues)

## License

By contributing to FluxION, you agree that your contributions will be licensed under the GNU Affero
General Public License v3.0 (AGPL-3.0).

For commercial licensing inquiries, contact: info@solare.cz

______________________________________________________________________

Thank you for contributing to FluxION! 🌞⚡
