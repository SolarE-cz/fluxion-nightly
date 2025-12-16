# Operating Modes
mode-self-use = Self Use
mode-force-charge = Force Charge
mode-force-discharge = Force Discharge
mode-backup = Backup Mode
mode-feed-in-priority = Feed-in Priority
mode-off-grid = Off-Grid Mode

# Topology
topology-independent = Independent
topology-master = Master ({ $count ->
    [one] { $count } slave
   *[other] { $count } slaves
})
topology-slave = Slave of { $master }

# Units
unit-percent = %
unit-watt = W
unit-kilowatt = kW
unit-kilowatt-hour = kWh
unit-voltage = V
unit-ampere = A
unit-celsius = °C
unit-hertz = Hz

# Status
status-online = Online
status-offline = Offline
status-error = Error
status-warning = Warning
status-ok = OK

# Common
yes = Yes
no = No
unknown = Unknown
not-available = N/A
