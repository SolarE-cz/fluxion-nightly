# FluxION Documentation

This is the single documentation root for the FluxION project. All active reference material lives
here. Historical development diaries are in [`archive/`](archive/).

______________________________________________________________________

## Architecture

System design and technical architecture.

| Document | Description | |----------|-------------| |
[ARCHITECTURE.md](architecture/ARCHITECTURE.md) | Primary system architecture — crates, ECS pattern,
data flow | | [ECONOMIC_OPTIMIZATION.md](architecture/ECONOMIC_OPTIMIZATION.md) | Economic
optimization model and decision logic | | [SCHEDULING.md](architecture/SCHEDULING.md) | Scheduling
system design | | [REMOTE_ACCESS.md](architecture/REMOTE_ACCESS.md) | Tor hidden service + mobile
remote access | | [BUILD_SYSTEM.md](architecture/BUILD_SYSTEM.md) | Nix flake build system analysis
|

______________________________________________________________________

## Guides

How-to documentation for users and developers.

| Document | Description | |----------|-------------| | [CONFIGURATION.md](guides/CONFIGURATION.md)
| Configuration file reference (`config.toml`) | | [DEPLOYMENT.md](guides/DEPLOYMENT.md) | Deploying
FluxION (HA addon, Docker, nightly) | | [TESTING.md](guides/TESTING.md) | Running tests and the test
suite | | [CUSTOM_STRATEGIES.md](guides/CUSTOM_STRATEGIES.md) | Writing and registering custom
strategies | | [WEB_UI.md](guides/WEB_UI.md) | Web UI features and usage | |
[NIX_DOCKER.md](guides/NIX_DOCKER.md) | Nix-based Docker image builds | | [I18N.md](guides/I18N.md)
| Internationalization |

______________________________________________________________________

## Reference

Technical reference for specific subsystems.

| Document | Description | |----------|-------------| | [STRATEGIES.md](reference/STRATEGIES.md) |
Strategy implementations — Winter Adaptive V9, morning precharge, battery prediction | |
[SCHEDULING_CONSTRAINTS.md](reference/SCHEDULING_CONSTRAINTS.md) | Scheduling constraint system —
architecture, quick reference, examples | | [SIMULATION.md](reference/SIMULATION.md) | Strategy
simulator — CLI reference and quick reference | |
[COST_CALCULATION.md](reference/COST_CALCULATION.md) | Cost calculation logic and self-use
optimization | | [TELEMETRY.md](reference/TELEMETRY.md) | Telemetry pipeline and heartbeat protocol
| | [SOLAX.md](reference/SOLAX.md) | SolaX inverter integration and register reference |

______________________________________________________________________

## Operations

CI/CD, release management, and the upgrader system.

| Document | Description | |----------|-------------| | [CI_CD.md](operations/CI_CD.md) | GitLab CI
pipeline and GitHub publish workflow | | [DEPLOYMENT_FLOW.md](operations/DEPLOYMENT_FLOW.md) |
Deployment flow, workflow, and verification | | [UPGRADER.md](operations/UPGRADER.md) | OTA upgrader
— distribution, crate design, migration |

______________________________________________________________________

## Vision

Future feature concepts.

| Document | Description | |----------|-------------| |
[Premium Subscription](vision/01_PREMIUM_SUBSCRIPTION_SERVICE.md) | Remote management SaaS concept |
| [Multi-View Dashboard](vision/02_MULTI_VIEW_TABLET_DASHBOARD.md) | Tablet-optimised dashboard
concept | | [Smart Device / AI](vision/03_SMART_DEVICE_INTEGRATION_AI_AUTOMATIONS.md) | AI
automation integration concept |

______________________________________________________________________

## Archive

Historical development diaries, implementation notes, and superseded documents are in
[`archive/`](archive/). Nothing is deleted — context is preserved for future reference.
