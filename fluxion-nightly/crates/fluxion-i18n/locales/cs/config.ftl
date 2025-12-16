# Konfigurace - Stránka
config-page-title = Konfigurace
config-page-subtitle = Správa nastavení FluxION
config-save-button = Uložit změny
config-cancel-button = Zrušit
config-reset-button = Obnovit výchozí
config-export-button = Exportovat konfiguraci
config-import-button = Importovat konfiguraci

# Systémová sekce
config-section-system = Systémová nastavení
config-section-system-desc = Obecná konfigurace systému a chování

config-system-debug-mode = Debug režim
config-system-debug-mode-help = Pokud je zapnuto, FluxION neprovede skutečné změny na měniči (bezpečný režim pro testování)

config-system-update-interval = Interval aktualizace
config-system-update-interval-help = Jak často FluxION kontroluje ceny a aktualizuje plán (v sekundách, minimum 10)

config-system-log-level = Úroveň logování
config-system-log-level-help = Podrobnost logování (error, warn, info, debug, trace)

config-system-display-currency = Zobrazená měna
config-system-display-currency-help = Měna pro zobrazení cen ve webovém rozhraní

config-system-language = Jazyk
config-system-language-help = Jazyk uživatelského rozhraní

# Sekce měničů
config-section-inverters = Konfigurace měničů
config-section-inverters-desc = Nastavení jednoho nebo více solárních měničů

config-inverter-add = Přidat měnič
config-inverter-remove = Odebrat
config-inverter-id = ID měniče
config-inverter-id-help = Jedinečný identifikátor tohoto měniče

config-inverter-type = Typ měniče
config-inverter-type-help = Značka/model měniče (např. Solax, Solax-Ultra)

config-inverter-entity-prefix = Prefix entity
config-inverter-entity-prefix-help = Prefix pro názvy entit v Home Assistant (např. "solax" pro sensor.solax_battery_soc)

config-inverter-topology = Topologie
config-inverter-topology-help = Jak se tento měnič vztahuje k ostatním (independent, master, nebo slave)

config-inverter-slaves = Podřízené měniče
config-inverter-slaves-help = ID podřízených měničů ovládaných tímto masterem

config-inverter-master = Nadřízený měnič
config-inverter-master-help = ID nadřízeného měniče ovládajícího tento slave

# Sekce cen
config-section-pricing = Konfigurace cen
config-section-pricing-desc = Nastavení zdrojů cen elektřiny a pevných cen

config-pricing-spot-entity = Entita spotových cen
config-pricing-spot-entity-help = ID entity v Home Assistant pro aktuální spotové ceny (např. sensor.current_spot_electricity_price_15min)

config-pricing-tomorrow-entity = Entita zítřejších cen
config-pricing-tomorrow-entity-help = Volitelné: samostatná entita pro zítřejší ceny

config-pricing-use-spot-buy = Používat spotové ceny pro nákup
config-pricing-use-spot-buy-help = Použít spotové ceny v reálném čase pro rozhodování o nabíjení

config-pricing-use-spot-sell = Používat spotové ceny pro prodej
config-pricing-use-spot-sell-help = Použít spotové ceny v reálném čase pro rozhodování o vybíjení

config-pricing-fixed-buy = Pevné nákupní ceny
config-pricing-fixed-buy-help = Záložní hodinové nákupní ceny když spotové ceny nejsou dostupné (24 hodnot v Kč/kWh)

config-pricing-fixed-sell = Pevné prodejní ceny
config-pricing-fixed-sell-help = Záložní hodinové prodejní ceny když spotové ceny nejsou dostupné (24 hodnot v Kč/kWh)

# Sekce řízení
config-section-control = Parametry řízení
config-section-control-desc = Nastavení provozu baterie a řízení

config-control-battery-capacity = Kapacita baterie
config-control-battery-capacity-help = Celková kapacita baterie v kWh

config-control-min-soc = Minimální SOC baterie
config-control-min-soc-help = Cílové minimum stavu nabití pro strategická rozhodnutí (%)

config-control-max-soc = Maximální SOC baterie
config-control-max-soc-help = Maximální povolený stav nabití (%)

config-control-hardware-min-soc = Hardwarové minimum SOC
config-control-hardware-min-soc-help = Absolutní minimum SOC vynucené firmwarem měniče (%)

config-control-battery-wear-cost = Náklady na opotřebení baterie
config-control-battery-wear-cost-help = Náklady na degradaci baterie na cyklovanou kWh (Kč/kWh)

config-control-battery-efficiency = Účinnost baterie
config-control-battery-efficiency-help = Účinnost celého cyklu (0.0 až 1.0, typicky: 0.90-0.95)

config-control-max-export-power = Maximální výkon exportu
config-control-max-export-power-help = Maximální výkon pro export do sítě (watty)

config-control-force-charge-hours = Hodiny nucen ého nabíjení
config-control-force-charge-hours-help = Počet nejlevnějších hodin denně pro nucené nabíjení baterie

config-control-force-discharge-hours = Hodiny nuceného vybíjení
config-control-force-discharge-hours-help = Počet nejdražších hodin denně pro nucené vybíjení baterie

config-control-min-mode-change-interval = Min. interval změny režimu
config-control-min-mode-change-interval-help = Minimální čas mezi změnami režimu pro zabránění rychlého přepínání (sekundy, minimum 60)

config-control-min-consecutive-blocks = Min. po sobě jdoucí bloky
config-control-min-consecutive-blocks-help = Minimální počet po sobě jdoucích 15minutových bloků pro nucené operace (zabraňuje nadměrnému zápisu do EEPROM)

config-control-default-battery-mode = Výchozí režim baterie
config-control-default-battery-mode-help = Režim baterie když není nucené nabíjení/vybíjení (SelfUse nebo BackUpMode)

config-control-average-load = Průměrná spotřeba domácnosti
config-control-average-load-help = Průměrná spotřeba v kW (použito pro predikce SOC)

# Sekce strategií
config-section-strategies = Konfigurace strategií
config-section-strategies-desc = Zapnutí/vypnutí a nastavení optimalizačních strategií

# Priorita strategií
config-strat-priority = Priorita
config-strat-priority-help = Priorita při řešení konfliktů (0-100, vyšší hodnota vyhrává při konfliktu strategií)

# Názvy strategií (krátká forma)
config-strat-winter-adaptive = Zimní adaptivní
config-strat-winter-adaptive-desc = Komplexní zimní strategie se sledováním spotřeby, cenovou arbitráží a chytrým nabíjením
config-strat-wa-ema-days = Období EMA (dny)
config-strat-wa-target-soc = Cílové SOC (%)

config-strat-winter-adaptive-v2 = Zimní adaptivní V2
config-strat-winter-adaptive-v2-desc = Nová generace zimní strategie s predikováním po slotech, detekcí cenových špiček a lepšími arbitrážními okny

config-strat-winter-peak-discharge = Zimní špičkové vybíjení
config-strat-winter-peak-discharge-desc = Vybíjení baterie během drahých ranních hodin před sluncem
config-strat-wpd-min-spread = Minimální rozpětí (Kč)
config-strat-wpd-min-soc-start = Minimální SOC pro start (%)

config-strat-solar-aware = Nabíjení s ohledem na solár
config-strat-solar-aware-desc = Vyhýbání se nabíjení ze sítě když se očekává solární výroba

config-strat-morning-precharge = Ranní přednabití
config-strat-day-ahead = Plánování den dopředu
config-strat-price-arbitrage = Cenová arbitráž

config-strategy-winter-peak = Zimní špičkové vybíjení
config-strategy-winter-peak-help = Vybít baterii během drahých zimních ranních hodin

config-strategy-winter-peak-min-spread = Minimální cenový rozdíl
config-strategy-winter-peak-min-spread-help = Minimální cenový rozdíl nutný k aktivaci (Kč)

config-strategy-winter-peak-min-soc-start = Minimální SOC pro start
config-strategy-winter-peak-min-soc-start-help = Minimální SOC baterie nutný k zahájení vybíjení (%)

config-strategy-winter-peak-min-soc-target = Cílové minimální SOC
config-strategy-winter-peak-min-soc-target-help = Cílové SOC pro vybití (%)

config-strategy-winter-peak-solar-window-start = Začátek solárního okna (hodina)
config-strategy-winter-peak-solar-window-start-help = Hodina kdy typicky začíná výroba ze solárů

config-strategy-winter-peak-solar-window-end = Konec solárního okna (hodina)
config-strategy-winter-peak-solar-window-end-help = Hodina kdy typicky končí výroba ze solárů

config-strategy-winter-peak-min-hours-to-solar = Min. hodin do solárního okna
config-strategy-winter-peak-min-hours-to-solar-help = Minimální počet hodin před solárním oknem k aktivaci

config-strategy-solar-aware = Nabíjení s ohledem na solár
config-strategy-solar-aware-help = Vyhnout se nabíjení když se očekává solární výroba

config-strategy-solar-aware-solar-window-start = Začátek solárního okna (hodina)
config-strategy-solar-aware-solar-window-end = Konec solárního okna (hodina)
config-strategy-solar-aware-midday-max-soc = Maximální SOC o poledni
config-strategy-solar-aware-midday-max-soc-help = Maximální SOC během solárních hodin pro ponechání místa pro solární nabíjení (%)

config-strategy-solar-aware-min-forecast = Minimální solární předpověď
config-strategy-solar-aware-min-forecast-help = Minimální očekávaná solární výroba k aktivaci (kWh)

config-strategy-morning-precharge = Ranní přednabití
config-strategy-morning-precharge-help = Nabít baterii v levných ranních hodinách před špičkou

config-strategy-day-ahead = Plánování den dopředu
config-strategy-day-ahead-help = Naplánovat celý denní rozvrh na základě zítřejších cen

config-strategy-time-aware = Časově orientované nabíjení
config-strategy-time-aware-help = Nabíjet během specifických časových oken

config-strategy-price-arbitrage = Cenová arbitráž
config-strategy-price-arbitrage-help = Nakupovat levně, prodávat draho na základě cenových rozdílů

config-strategy-solar-first = Solár přednostně
config-strategy-solar-first-help = Upřednostnit solární výrobu před nabíjením ze sítě

config-strategy-self-use = Vlastní spotřeba
config-strategy-self-use-help = Maximalizovat vlastní spotřebu solární energie

config-strategy-seasonal-force = Vynucené roční období
config-strategy-seasonal-force-help = Přepsat automatickou detekci ročního období (ponechat prázdné pro auto)

# Validační zprávy
config-validation-required = Toto pole je povinné
config-validation-min = Hodnota musí být alespoň {$min}
config-validation-max = Hodnota musí být nejvýše {$max}
config-validation-range = Hodnota musí být mezi {$min} a {$max}
config-validation-positive = Hodnota musí být kladná
config-validation-non-negative = Hodnota musí být nezáporná

# Zprávy o úspěchu/chybě
config-save-success = Konfigurace úspěšně uložena
config-save-error = Nepodařilo se uložit konfiguraci
config-validation-error = Konfigurace obsahuje chyby validace
config-restart-required = Některé změny vyžadují restart aby se projevily
config-backup-created = Záloha vytvořena: {$backup_id}
config-restore-success = Konfigurace obnovena ze zálohy
