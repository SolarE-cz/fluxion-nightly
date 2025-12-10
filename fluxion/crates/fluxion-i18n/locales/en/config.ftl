# Configuration Page
config-page-title = Configuration
config-page-subtitle = Manage FluxION settings
config-save-button = Save Changes
config-cancel-button = Cancel
config-reset-button = Reset to Defaults
config-export-button = Export Configuration
config-import-button = Import Configuration

# System Section
config-section-system = System Settings
config-section-system-desc = General system configuration and behavior

config-system-debug-mode = Debug Mode
config-system-debug-mode-help = When enabled, FluxION will not make actual changes to your inverter (safe mode for testing)

config-system-update-interval = Update Interval
config-system-update-interval-help = How often FluxION checks prices and updates schedule (in seconds, minimum 10)

config-system-log-level = Log Level
config-system-log-level-help = Verbosity of logging (error, warn, info, debug, trace)

config-system-display-currency = Display Currency
config-system-display-currency-help = Currency to use for displaying prices in the web UI

config-system-language = Language
config-system-language-help = User interface language

# Inverter Section
config-section-inverters = Inverter Configuration
config-section-inverters-desc = Configure one or more solar inverters

config-inverter-add = Add Inverter
config-inverter-remove = Remove
config-inverter-id = Inverter ID
config-inverter-id-help = Unique identifier for this inverter

config-inverter-type = Inverter Type
config-inverter-type-help = Brand/model of the inverter (e.g., Solax, Solax-Ultra)

config-inverter-entity-prefix = Entity Prefix
config-inverter-entity-prefix-help = Prefix for Home Assistant entity names (e.g., "solax" for sensor.solax_battery_soc)

config-inverter-topology = Topology
config-inverter-topology-help = How this inverter relates to others (independent, master, or slave)

config-inverter-slaves = Slave Inverters
config-inverter-slaves-help = IDs of slave inverters controlled by this master

config-inverter-master = Master Inverter
config-inverter-master-help = ID of the master inverter controlling this slave

# Pricing Section
config-section-pricing = Pricing Configuration
config-section-pricing-desc = Configure electricity price sources and fixed prices

config-pricing-spot-entity = Spot Price Entity
config-pricing-spot-entity-help = Home Assistant entity ID for current spot prices (e.g., sensor.current_spot_electricity_price_15min)

config-pricing-tomorrow-entity = Tomorrow Price Entity
config-pricing-tomorrow-entity-help = Optional: separate entity for tomorrow's prices

config-pricing-use-spot-buy = Use Spot Prices for Buying
config-pricing-use-spot-buy-help = Use real-time spot prices for charge decisions

config-pricing-use-spot-sell = Use Spot Prices for Selling
config-pricing-use-spot-sell-help = Use real-time spot prices for discharge decisions

config-pricing-fixed-buy = Fixed Buy Prices
config-pricing-fixed-buy-help = Fallback hourly buy prices when spot prices unavailable (24 values in CZK/kWh)

config-pricing-fixed-sell = Fixed Sell Prices
config-pricing-fixed-sell-help = Fallback hourly sell prices when spot prices unavailable (24 values in CZK/kWh)

# Control Section
config-section-control = Control Parameters
config-section-control-desc = Battery operation and control settings

config-control-battery-capacity = Battery Capacity
config-control-battery-capacity-help = Total battery capacity in kWh

config-control-min-soc = Minimum Battery SOC
config-control-min-soc-help = Target minimum state of charge for strategy decisions (%)

config-control-max-soc = Maximum Battery SOC
config-control-max-soc-help = Maximum allowed state of charge (%)

config-control-hardware-min-soc = Hardware Minimum SOC
config-control-hardware-min-soc-help = Absolute minimum SOC enforced by inverter firmware (%)

config-control-battery-wear-cost = Battery Wear Cost
config-control-battery-wear-cost-help = Cost of battery degradation per kWh cycled (CZK/kWh)

config-control-battery-efficiency = Battery Efficiency
config-control-battery-efficiency-help = Round-trip efficiency (0.0 to 1.0, typical: 0.90-0.95)

config-control-max-export-power = Maximum Export Power
config-control-max-export-power-help = Maximum power to export to grid (watts)

config-control-force-charge-hours = Force Charge Hours
config-control-force-charge-hours-help = Number of cheapest hours per day to force-charge battery

config-control-force-discharge-hours = Force Discharge Hours
config-control-force-discharge-hours-help = Number of most expensive hours per day to force-discharge battery

config-control-min-mode-change-interval = Min Mode Change Interval
config-control-min-mode-change-interval-help = Minimum time between mode changes to prevent rapid switching (seconds, minimum 60)

config-control-min-consecutive-blocks = Min Consecutive Force Blocks
config-control-min-consecutive-blocks-help = Minimum number of consecutive 15-minute blocks for force operations (prevents excessive EEPROM writes)

config-control-default-battery-mode = Default Battery Mode
config-control-default-battery-mode-help = Battery mode when not force charging/discharging (SelfUse or BackUpMode)

config-control-average-load = Average Household Load
config-control-average-load-help = Average power consumption in kW (used for SOC predictions)

# Strategy Section
config-section-strategies = Strategy Configuration
config-section-strategies-desc = Enable/disable and configure optimization strategies

config-strategy-winter-peak = Winter Peak Discharge
config-strategy-winter-peak-help = Discharge battery during expensive winter morning hours

config-strategy-winter-peak-min-spread = Minimum Price Spread
config-strategy-winter-peak-min-spread-help = Minimum price difference required to activate (CZK)

config-strategy-winter-peak-min-soc-start = Minimum SOC to Start
config-strategy-winter-peak-min-soc-start-help = Minimum battery SOC required to begin discharge (%)

config-strategy-winter-peak-min-soc-target = Minimum SOC Target
config-strategy-winter-peak-min-soc-target-help = Target SOC to discharge down to (%)

config-strategy-winter-peak-solar-window-start = Solar Window Start Hour
config-strategy-winter-peak-solar-window-start-help = Hour when solar generation typically begins

config-strategy-winter-peak-solar-window-end = Solar Window End Hour
config-strategy-winter-peak-solar-window-end-help = Hour when solar generation typically ends

config-strategy-winter-peak-min-hours-to-solar = Min Hours to Solar
config-strategy-winter-peak-min-hours-to-solar-help = Minimum hours before solar window to activate

config-strategy-solar-aware = Solar Aware Charging
config-strategy-solar-aware-help = Avoid charging when solar generation is expected

config-strategy-solar-aware-solar-window-start = Solar Window Start Hour
config-strategy-solar-aware-solar-window-end = Solar Window End Hour
config-strategy-solar-aware-midday-max-soc = Midday Maximum SOC
config-strategy-solar-aware-midday-max-soc-help = Maximum SOC during solar hours to leave room for solar charging (%)

config-strategy-solar-aware-min-forecast = Minimum Solar Forecast
config-strategy-solar-aware-min-forecast-help = Minimum expected solar generation to activate (kWh)

config-strategy-morning-precharge = Morning Precharge
config-strategy-morning-precharge-help = Charge battery in cheap morning hours before peak

config-strategy-day-ahead = Day Ahead Planning
config-strategy-day-ahead-help = Plan full day schedule based on tomorrow's prices

config-strategy-time-aware = Time Aware Charge
config-strategy-time-aware-help = Charge during specific time windows

config-strategy-price-arbitrage = Price Arbitrage
config-strategy-price-arbitrage-help = Buy low, sell high based on price differences

config-strategy-solar-first = Solar First
config-strategy-solar-first-help = Prioritize solar generation over grid charging

config-strategy-self-use = Self Use
config-strategy-self-use-help = Maximize self-consumption of solar energy

config-strategy-seasonal-force = Force Season
config-strategy-seasonal-force-help = Override automatic season detection (leave empty for auto)

# Validation Messages
config-validation-required = This field is required
config-validation-min = Value must be at least {$min}
config-validation-max = Value must be at most {$max}
config-validation-range = Value must be between {$min} and {$max}
config-validation-positive = Value must be positive
config-validation-non-negative = Value must be non-negative

# Success/Error Messages
config-save-success = Configuration saved successfully
config-save-error = Failed to save configuration
config-validation-error = Configuration has validation errors
config-restart-required = Some changes require restart to take effect
config-backup-created = Backup created: {$backup_id}
config-restore-success = Configuration restored from backup
