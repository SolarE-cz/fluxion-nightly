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

# ==============================================================================
# Upgrader Configuration (Phase 5.1: Transition Mode)
# ==============================================================================
UPGRADER_CONFIG_PATH="/data/upgrader_config.json"
FLUXION_UPGRADER_BIN="/usr/local/bin/fluxion-upgrader"

# Initialize upgrader config from HA addon options on first boot
if [ ! -f "$UPGRADER_CONFIG_PATH" ]; then
  bashio::log.info "Creating upgrader configuration..."

  AUTO_UPDATE=$(bashio::config 'auto_update' 'true')
  RELEASE_BRANCH=$(bashio::config 'release_branch' 'nightly')
  STAGING_TOKEN=$(bashio::config 'staging_token' '')

  if [ -n "$STAGING_TOKEN" ]; then
    TOKEN_JSON="\"$STAGING_TOKEN\""
  else
    TOKEN_JSON="null"
  fi

  cat >"$UPGRADER_CONFIG_PATH" <<EOF
{
  "auto_update": $AUTO_UPDATE,
  "release_branch": "$RELEASE_BRANCH",
  "staging_token": $TOKEN_JSON,
  "check_interval_secs": 3600,
  "fluxion_port": 8099,
  "max_calm_wait_hours": 6
}
EOF
  bashio::log.info "Upgrader config created at $UPGRADER_CONFIG_PATH"
fi

# Display upgrader configuration
if [ -f "$UPGRADER_CONFIG_PATH" ]; then
  UPGRADER_AUTO_UPDATE=$(jq -r '.auto_update' "$UPGRADER_CONFIG_PATH")
  UPGRADER_RELEASE_BRANCH=$(jq -r '.release_branch' "$UPGRADER_CONFIG_PATH")

  bashio::log.info "Upgrader configuration:"
  bashio::log.info "  Auto update: $UPGRADER_AUTO_UPDATE"
  bashio::log.info "  Release branch: $UPGRADER_RELEASE_BRANCH"
fi

# ==============================================================================
# Start Binary (Phase 5.1: Transition - Check for upgrader)
# ==============================================================================
# Check if upgrader mode is available and enabled
if [ -f "$FLUXION_UPGRADER_BIN" ] && [ -f "$UPGRADER_CONFIG_PATH" ]; then
  UPGRADER_AUTO_UPDATE=$(jq -r '.auto_update' "$UPGRADER_CONFIG_PATH")

  if [ "$UPGRADER_AUTO_UPDATE" = "true" ]; then
    bashio::log.info "Starting in upgrader mode..."
    exec "$FLUXION_UPGRADER_BIN"
  fi
fi

# Start in legacy mode
bashio::log.info "Starting FluxION binary (legacy mode)..."
exec /usr/local/bin/fluxion-main
