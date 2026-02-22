# Režimy provozu
mode-self-use = Vlastní spotřeba
mode-force-charge = Nucené nabíjení
mode-force-discharge = Nucené vybíjení
mode-backup = Záložní režim
mode-feed-in-priority = Priorita výkupu
mode-off-grid = Ostrovní režim

# Topologie
topology-independent = Nezávislý
topology-master = Hlavní ({ $count ->
    [one] { $count } podřízený
    [few] { $count } podřízené
   *[other] { $count } podřízených
})
topology-slave = Podřízený { $master }

# Jednotky
unit-percent = %
unit-watt = W
unit-kilowatt = kW
unit-kilowatt-hour = kWh
unit-voltage = V
unit-ampere = A
unit-celsius = °C
unit-hertz = Hz

# Stav
status-online = Online
status-offline = Offline
status-error = Chyba
status-warning = Varování
status-ok = OK

# Společné
yes = Ano
no = Ne
unknown = Neznámý
not-available = Není k dispozici
