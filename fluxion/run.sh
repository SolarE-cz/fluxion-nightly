#!/usr/bin/with-contenv bashio
# ==============================================================================
# FluxION ECS Startup Script for Home Assistant Addon
# ==============================================================================

bashio::log.info "Starting FluxION ECS..."

# Check if running in Home Assistant Supervisor
if bashio::supervisor.ping; then
  bashio::log.info "Running in Home Assistant Supervisor mode"
  export SUPERVISOR_TOKEN="${SUPERVISOR_TOKEN}"
else
  bashio::log.warning "Not running in Home Assistant Supervisor"
  bashio::log.warning "Make sure HA_BASE_URL and HA_TOKEN environment variables are set"
fi

# Get configuration from addon options
CONFIG_PATH="/data/options.json"

if [ -f "$CONFIG_PATH" ]; then
  bashio::log.info "Loading configuration from $CONFIG_PATH"
else
  bashio::log.warning "No configuration file found at $CONFIG_PATH"
  bashio::log.info "Using default configuration"
fi

# Set log level from addon config (default: info)
LOG_LEVEL=$(bashio::config 'log_level' 'info')
export RUST_LOG="${LOG_LEVEL}"

bashio::log.info "Log level: $LOG_LEVEL"

# Display configuration summary
if [ -f "$CONFIG_PATH" ]; then
  DEBUG_MODE=$(jq -r '.debug_mode // true' "$CONFIG_PATH")
  INVERTER_COUNT=$(jq -r '.inverters | length' "$CONFIG_PATH")

  bashio::log.info "Configuration summary:"
  bashio::log.info "  Debug mode: $DEBUG_MODE"
  bashio::log.info "  Inverters configured: $INVERTER_COUNT"

  if [ "$DEBUG_MODE" = "true" ]; then
    bashio::log.warning "🔍 DEBUG MODE ENABLED - No real changes will be made to inverters"
    bashio::log.warning "🔍 Set debug_mode: false in configuration to enable production mode"
  else
    bashio::log.warning "⚠️  PRODUCTION MODE - System will make REAL changes to inverters!"
  fi
fi

# Start FluxION
bashio::log.info "Starting FluxION binary..."
exec /usr/local/bin/fluxion-main
