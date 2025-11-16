# FluxION Documentation

Welcome to the FluxION documentation! This directory contains comprehensive documentation organized
by topic.

## 📚 Documentation Structure

### 🏗️ Architecture

High-level system design and architectural decisions.

- [**ARCHITECTURE.md**](architecture/ARCHITECTURE.md) - Complete system architecture overview
  - ECS (Entity Component System) design
  - Vendor-agnostic sensor architecture
  - Component and system descriptions
  - Data flow and communication patterns

### 📖 User Guides

Step-by-step guides for setting up and using FluxION.

- [**CONFIGURATION.md**](guides/CONFIGURATION.md) - Configuration file format and options
- [**DEPLOYMENT.md**](guides/DEPLOYMENT.md) - Deployment guide (Docker, Home Assistant addon)
- [**I18N.md**](guides/I18N.md) - Internationalization guide (adding translations)
- [**TESTING.md**](guides/TESTING.md) - Testing guide (unit, integration tests)
- [**WEB_UI_GUIDE.md**](guides/WEB_UI_GUIDE.md) - Web UI usage and features

### 🔧 Development History

Historical documents tracking feature development and improvements.

- [**ASYNC_MIGRATION_COMPLETE.md**](development/ASYNC_MIGRATION_COMPLETE.md) - Async architecture
  migration
- [**BATTERY_PREDICTION_IMPROVEMENTS.md**](development/BATTERY_PREDICTION_IMPROVEMENTS.md) - Battery
  SOC prediction enhancements
- [**CHART_ENHANCEMENTS.md**](development/CHART_ENHANCEMENTS.md) - Web UI chart improvements
- [**ECS_AND_WEB_UI_WORKING.md**](development/ECS_AND_WEB_UI_WORKING.md) - ECS + Web UI integration
  milestone
- [**EXPORT_FEATURE.md**](development/EXPORT_FEATURE.md) - State export feature implementation
- [**FRAGMENTED_CHARGING_FIX.md**](development/FRAGMENTED_CHARGING_FIX.md) - Charging fragmentation
  fixes
- [**MODBUS_CLEANUP_PLAN.md**](development/MODBUS_CLEANUP_PLAN.md) - Modbus code cleanup
- [**OPTIMIZATION_ANALYSIS.md**](development/OPTIMIZATION_ANALYSIS.md) - Performance optimization
  analysis
- [**OPTIMIZATION_SUMMARY.md**](development/OPTIMIZATION_SUMMARY.md) - Optimization results summary
- [**REFACTORING_SUMMARY.md**](development/REFACTORING_SUMMARY.md) - Code refactoring summary
- [**STRATEGY_ENHANCEMENT.md**](development/STRATEGY_ENHANCEMENT.md) - Strategy system enhancements
- [**WARP.md**](development/WARP.md) - Web framework notes (Warp → Axum migration)
- [**WINTER_STRATEGY_IMPLEMENTATION.md**](development/WINTER_STRATEGY_IMPLEMENTATION.md) - Winter
  optimization strategy

### 📋 Planning & Status

Project planning documents and status reports.

- [**DEBUG_LOGGING.md**](planning/DEBUG_LOGGING.md) - Debug logging implementation plan
- [**DOCUMENTATION_SUMMARY.md**](planning/DOCUMENTATION_SUMMARY.md) - Documentation status summary
- [**IMPLEMENTATION_PLAN.md**](planning/IMPLEMENTATION_PLAN.md) - Original implementation roadmap
- [**MVP_STATUS.md**](planning/MVP_STATUS.md) - MVP milestone status
- [**PROJECT_STATUS.md**](planning/PROJECT_STATUS.md) - Overall project status

## 🚀 Quick Links

### For Users

- **Getting Started:** See [README.md](../README.md) in the repository root
- **Configuration:** [guides/CONFIGURATION.md](guides/CONFIGURATION.md)
- **Deployment:** [guides/DEPLOYMENT.md](guides/DEPLOYMENT.md)

### For Developers

- **Architecture:** [architecture/ARCHITECTURE.md](architecture/ARCHITECTURE.md)
- **Testing:** [guides/TESTING.md](guides/TESTING.md)
- **Contributing:** See CONTRIBUTING.md (to be created)

### For Translators

- **Internationalization:** [guides/I18N.md](guides/I18N.md)

## 📝 Document Categories

### User-Facing Documentation

These documents are intended for end users:

- Configuration guides
- Deployment instructions
- Web UI usage
- Internationalization

### Developer Documentation

These documents are for contributors and maintainers:

- Architecture overviews
- Testing guides
- Development history
- Planning documents

## 🔍 Finding What You Need

### "How do I configure FluxION?"

→ [guides/CONFIGURATION.md](guides/CONFIGURATION.md)

### "How do I deploy FluxION?"

→ [guides/DEPLOYMENT.md](guides/DEPLOYMENT.md)

### "How does the system work internally?"

→ [architecture/ARCHITECTURE.md](architecture/ARCHITECTURE.md)

### "How do I add a new language?"

→ [guides/I18N.md](guides/I18N.md)

### "What features were recently added?"

→ Check the development history files in [development/](development/)

### "What's the project status?"

→ [planning/PROJECT_STATUS.md](planning/PROJECT_STATUS.md)

## 📅 Document Maintenance

- **Last major reorganization:** 2025-10-29
- **Documentation style:** Markdown with GitHub flavor
- **Update frequency:** As needed when features change

## 🤝 Contributing to Documentation

If you find errors or want to improve the documentation:

1. Check if the document is user-facing (guides) or internal (development/planning)
2. For user guides: Keep language clear and concise
3. For architecture docs: Include diagrams where helpful
4. For development history: Document the "why" not just the "what"

______________________________________________________________________

**Note:** Some documents in the development/ and planning/ directories are historical records and
may not reflect the current implementation. Always check the
[ARCHITECTURE.md](architecture/ARCHITECTURE.md) and user guides for current information.
