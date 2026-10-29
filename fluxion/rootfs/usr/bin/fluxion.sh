#!/command/with-contenv bashio
# shellcheck shell=bash
# ==============================================================================
# Home Assistant Add-on: FluxION ECS
#
# Main FluxION ECS execution script
# ==============================================================================

# Parse configuration
CONFIG_PATH=/data/options.json

# Export configuration as environment variables for the Rust binary
export DEBUG_MODE=$(bashio::config 'debug_mode')
export LOG_LEVEL=$(bashio::config 'log_level')
export SUPERVISOR_TOKEN="${SUPERVISOR_TOKEN}"
export HASSIO_TOKEN="${SUPERVISOR_TOKEN}"
export CONFIG_PATH="${CONFIG_PATH}"

bashio::log.info "Debug mode: ${DEBUG_MODE}"
bashio::log.info "Log level: ${LOG_LEVEL}"
bashio::log.info "Starting FluxION ECS binary..."

# Run the FluxION binary
exec /usr/local/bin/fluxion
