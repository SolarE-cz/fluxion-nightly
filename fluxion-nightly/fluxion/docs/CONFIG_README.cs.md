# FluxION - Průvodce konfigurací

Kompletní referenční příručka pro konfiguraci systému automatizace baterie FluxION.

## Obsah

- [Umístění konfiguračních souborů](#um%C3%ADst%C4%9Bn%C3%AD-konfigura%C4%8Dn%C3%ADch-soubor%C5%AF)
- [Formát konfigurace](#form%C3%A1t-konfigurace)
- [Konfigurace střídačů](#konfigurace-st%C5%99%C3%ADda%C4%8D%C5%AF)
- [Konfigurace cen](#konfigurace-cen)
- [Konfigurace řízení](#konfigurace-%C5%99%C3%ADzen%C3%AD)
- [Konfigurace systému](#konfigurace-syst%C3%A9mu)
- [Konfigurace strategií](#konfigurace-strategi%C3%AD)
- [Proměnné prostředí](#prom%C4%9Bnn%C3%A9-prost%C5%99ed%C3%AD)
- [Kompletní příklady](#kompletn%C3%AD-p%C5%99%C3%ADklady)
- [Validační pravidla](#valida%C4%8Dn%C3%AD-pravidla)
- [Řešení problémů](#%C5%99e%C5%A1en%C3%AD-probl%C3%A9m%C5%AF)

## Umístění konfiguračních souborů

FluxION načítá konfiguraci z prvního dostupného zdroje v tomto pořadí:

1. **`/data/options.json`** - Možnosti doplňku Home Assistant (formát JSON, auto-generováno)
2. **`config.toml`** - Lokální konfigurační soubor TOML (doporučeno pro vývoj)
3. **`config.json`** - Lokální konfigurační soubor JSON
4. **Proměnné prostředí** - Mohou přepsat konkrétní nastavení
5. **Výchozí hodnoty** - Bezpečné výchozí hodnoty s povoleným debug režimem

## Formát konfigurace

FluxION podporuje dva formáty:

- **TOML** (doporučeno) - Uživatelsky přívětivý, podporuje komentáře
- **JSON** - Strojově přívětivý, používán doplňkem Home Assistant

Všechny níže uvedené příklady používají formát TOML. Pro ekvivalent JSON odstraňte komentáře a
převeďte na syntaxi JSON.

## Konfigurace střídačů

Definujte jeden nebo více střídačů. Alespoň jeden střídač je povinný.

### Základní střídač

```toml
[[inverters]]
id = "main_inverter"           # Unikátní identifikátor pro tento střídač
vendor = "solax"                # Značka střídače: solax, fronius, sma
entity_prefix = "solax"         # Prefix entity v Home Assistant
topology = "independent"        # Topologie: independent, master, slave
```

### Konfigurační pole

#### `id` (povinné, řetězec)

- Unikátní identifikátor pro tento střídač
- Používá se interně pro odkazování na tento střídač
- Příklad: `"main_inverter"`, `"master_inv"`, `"slave_1"`

#### `vendor` (povinné, řetězec)

- Výrobce/značka střídače
- Podporované hodnoty:
  - `"solax"` - Střídače Solax Power (testováno s X3-Hybrid G4)
  - `"fronius"` - Střídače Fronius (plánováno)
  - `"sma"` - Střídače SMA (plánováno)

#### `entity_prefix` (povinné, řetězec)

- Prefix používaný pro entity Home Assistant
- FluxION bude hledat entity jako `sensor.{prefix}_battery_soc`
- Musí odpovídat pojmenování entit vaší integrace Home Assistant
- Příklad: pokud máte v HA `sensor.solax_battery_soc`, použijte `entity_prefix = "solax"`

#### `topology` (povinné, řetězec)

- Definuje, jak se tento střídač vztahuje k ostatním
- Hodnoty:
  - `"independent"` - Jeden střídač nebo více nezávislých střídačů
  - `"master"` - Řídí jeden nebo více slave střídačů
  - `"slave"` - Řízen master střídačem

### Topologie více střídačů

#### Více nezávislých střídačů

Každý střídač pracuje nezávisle:

```toml
[[inverters]]
id = "inverter_1"
vendor = "solax"
entity_prefix = "solax_1"
topology = "independent"

[[inverters]]
id = "inverter_2"
vendor = "solax"
entity_prefix = "solax_2"
topology = "independent"
```

#### Konfigurace Master/Slave

Jeden master koordinuje více slave jednotek:

```toml
[[inverters]]
id = "master_inv"
vendor = "solax"
entity_prefix = "solax_master"
topology = "master"
slaves = ["slave_1", "slave_2"]    # Seznam ID slave střídačů

[[inverters]]
id = "slave_1"
vendor = "solax"
entity_prefix = "solax_slave1"
topology = "slave"
master = "master_inv"              # Odkaz na ID master střídače

[[inverters]]
id = "slave_2"
vendor = "solax"
entity_prefix = "solax_slave2"
topology = "slave"
master = "master_inv"
```

## Konfigurace cen

Nastavení cen elektřiny pro optimalizační rozhodnutí.

```toml
[pricing]
# Senzor Home Assistant poskytující aktuální spotovou cenu elektřiny
spot_price_entity = "sensor.current_spot_electricity_price_15min"

# Používat spotové ceny pro rozhodnutí o nákupu
use_spot_prices_to_buy = true

# Používat spotové ceny pro rozhodnutí o prodeji
use_spot_prices_to_sell = true

# Fixní hodinové ceny (24 hodnot) - používáno jako záloha při nedostupnosti spotových cen
fixed_buy_prices = [
    0.05, 0.05, 0.05, 0.05, 0.05, 0.05,  # 00:00-05:59 (noc)
    0.06, 0.07, 0.08, 0.08, 0.07, 0.06,  # 06:00-11:59 (ráno)
    0.06, 0.07, 0.08, 0.08, 0.09, 0.10,  # 12:00-17:59 (odpoledne)
    0.09, 0.08, 0.07, 0.06, 0.05, 0.05   # 18:00-23:59 (večer)
]

fixed_sell_prices = [
    0.08, 0.08, 0.08, 0.08, 0.08, 0.08,  # 00:00-05:59
    0.09, 0.10, 0.11, 0.11, 0.10, 0.09,  # 06:00-11:59
    0.09, 0.10, 0.11, 0.11, 0.12, 0.13,  # 12:00-17:59
    0.12, 0.11, 0.10, 0.09, 0.08, 0.08   # 18:00-23:59
]
```

### Konfigurační pole

#### `spot_price_entity` (povinné, řetězec)

- ID entity Home Assistant poskytující aktuální spotovou cenu elektřiny
- Běžné příklady:
  - `"sensor.current_spot_electricity_price_15min"`
  - `"sensor.nordpool_kwh_fi_eur_3_10_0"`
  - `"sensor.spot_price_kwh"`
- Musí poskytovat cenu v měně za kWh

#### `use_spot_prices_to_buy` (povinné, boolean)

- `true` - Používat real-time spotové ceny pro rozhodnutí o nabíjení
- `false` - Používat fixed_buy_prices pro rozhodnutí o nabíjení

#### `use_spot_prices_to_sell` (povinné, boolean)

- `true` - Používat real-time spotové ceny pro rozhodnutí o vybíjení/exportu
- `false` - Používat fixed_sell_prices pro rozhodnutí o vybíjení

#### `fixed_buy_prices` (povinné, pole čísel)

- Záložní ceny pro nákup elektřiny (nabíjení baterie)
- Ceny v místní měně za kWh (např. CZK/kWh, EUR/kWh)
- Můžete poskytnout:
  - **24 hodnot** - jedna cena za hodinu (bude rozšířeno na 96 15minutových bloků)
  - **96 hodnot** - jedna cena za 15minutový blok (pro přesné řízení)
- Používá se když:
  - `use_spot_prices_to_buy = false`
  - Senzor spotové ceny není dostupný
  - Data spotové ceny jsou zastaralá

#### `fixed_sell_prices` (povinné, pole čísel)

- Záložní ceny pro prodej elektřiny (vybíjení baterie nebo export do sítě)
- Stejný formát jako `fixed_buy_prices`
- Obvykle vyšší než nákupní ceny (výkupní cena)

### Poznámky k cenové strategii

**Používání spotových cen** (doporučeno):

- Povolte `use_spot_prices_to_buy` i `use_spot_prices_to_sell`
- FluxION bude optimalizovat na základě real-time cenových fluktuací
- Nejlepší pro trhy s volatilními spotovými cenami (např. severské země)

**Používání fixních cen**:

- Vypněte spotové ceny a spoléhejte na `fixed_buy_prices`/`fixed_sell_prices`
- Užitečné pro:
  - Smlouvy s fixní sazbou
  - Tarify s časovými pásmy (TOU)
  - Trhy bez přístupu ke spotovým cenám

**Hybridní přístup**:

- Povolit spot pro nákup, zakázat pro prodej (nebo naopak)
- Užitečné pokud máte fixní výkupní cenu, ale variabilní nákupní ceny

## Konfigurace řízení

Parametry řízení baterie a provozní limity.

```toml
[control]
# Maximální výkon exportu do sítě (watty)
maximum_export_power_w = 5000

# Počet nejlevnějších cenových období pro vynucené nabíjení baterie
force_charge_hours = 4

# Počet nejdražších cenových období pro vynucené vybíjení baterie
force_discharge_hours = 2

# Minimální stav nabití baterie pro strategická rozhodnutí (%)
min_battery_soc = 15.0

# Maximální stav nabití baterie (%)
max_battery_soc = 100.0

# Hardwarové minimum SOC vynucované firmwarem střídače (%)
hardware_min_battery_soc = 10.0

# Kapacita baterie v kWh
battery_capacity_kwh = 23.0

# Náklady na opotřebení baterie za kWh cyklu (měna/kWh)
battery_wear_cost_czk_per_kwh = 0.125

# Účinnost baterie při nabíjení a vybíjení (0.0 až 1.0)
battery_efficiency = 0.95

# Minimální čas mezi změnami režimu (sekundy)
min_mode_change_interval_secs = 300

# Průměrná spotřeba domácnosti (kW) - záloha pro predikce
average_household_load_kw = 0.5

# Minimální počet po sobě jdoucích 15minutových bloků pro vynucené operace
min_consecutive_force_blocks = 2
```

### Konfigurační pole

#### `maximum_export_power_w` (povinné, celé číslo)

- Maximální výkon povolený pro export do sítě ve wattech
- Typické hodnoty: 3000-10000W v závislosti na:
  - Kapacitě připojení k síti
  - Maximálním exportním výkonu střídače
  - Limitech dodavatele elektřiny
- Příklad: `5000` = max 5 kW export do sítě

#### `force_charge_hours` (povinné, celé číslo)

- Kolik nejlevnějších cenových období vynutit nabíjení baterie
- Rozsah: 0-24
- `0` = žádné vynucené nabíjení (baterie se nabíjí pouze ze solárních panelů)
- `4` = nabíjet během 4 nejlevnějších hodin dne
- Vyšší hodnoty = agresivnější nabíjení ze sítě

#### `force_discharge_hours` (povinné, celé číslo)

- Kolik nejdražších cenových období vynutit vybíjení baterie
- Rozsah: 0-24
- `0` = žádné vynucené vybíjení (baterie jen pro vlastní spotřebu)
- `2` = vybíjet během 2 nejdražších hodin
- Vyšší hodnoty = agresivnější arbitrážní obchodování

#### `min_battery_soc` (povinné, desetinné číslo, 0-100)

- Minimální stav nabití baterie pro strategická rozhodnutí
- Toto je cílové minimum - strategie se budou snažit udržet SOC nad touto hodnotou
- Typické hodnoty: 10-20%
- Mělo by být >= `hardware_min_battery_soc`
- Příklad: `15.0` = strategie se vyhýbají vybíjení pod 15%

#### `max_battery_soc` (povinné, desetinné číslo, 0-100)

- Maximální stav nabití baterie
- Horní limit pro nabíjecí operace
- Typické hodnoty: 90-100%
- Nastavení pod 100% může prodloužit životnost baterie
- Příklad: `100.0` = povolit nabíjení na plnou kapacitu

#### `hardware_min_battery_soc` (povinné, desetinné číslo, 0-100)

- Absolutní minimum SOC vynucované firmwarem střídače
- Načteno z nastavení střídače (např. Solax: `number.solax_selfuse_discharge_min_soc`)
- Nemůže být přepsáno FluxION
- Výchozí: `10.0`
- Mělo by být \<= `min_battery_soc`

#### `battery_capacity_kwh` (povinné, desetinné číslo)

- Celková kapacita baterie v kilowatthodinách
- Používá se pro:
  - Predikce SOC
  - Ekonomické výpočty
  - Odhady nákladů na opotřebení
- Příklad: `23.0` pro systém baterie 23 kWh
- Zkontrolujte specifikace vaší baterie pro přesnou hodnotu

#### `battery_wear_cost_czk_per_kwh` (povinné, desetinné číslo)

- Náklady na degradaci baterie za kWh cyklu
- V zobrazované měně za kWh
- Používá se pro ekonomickou optimalizaci (náklady na opotřebení vs. zisk z arbitráže)
- Příklad výpočtu:
  - Cena baterie: 115 000 CZK
  - Kapacita: 23 kWh
  - Životnost v cyklech: 6 000 cyklů
  - Cena za cyklus: 115 000 / 6 000 = 19,17 CZK
  - Cena za kWh: 19,17 / 23 = 0,833 CZK/kWh plný cyklus
  - Konzervativní odhad: ~0,125 CZK/kWh (s ohledem na částečné cykly)
- Výchozí: `0.125`

#### `battery_efficiency` (povinné, desetinné číslo, 0.0-1.0)

- Účinnost baterie při nabíjení a vybíjení (roundtrip)
- Zohledňuje ztráty energie v baterii a střídači
- Typicky lithium-ion: 0.90-0.95 (90-95%)
- Příklad: `0.95` = 5% ztráta energie za cyklus
- Používá se pro ekonomické výpočty

#### `min_mode_change_interval_secs` (povinné, celé číslo)

- Minimální čas v sekundách mezi změnami režimu střídače
- Zabraňuje rychlému přepínání, které může:
  - Opotřebovat EEPROM střídače
  - Způsobit nestabilitu
  - Snížit účinnost
- Minimální povolená hodnota: 60 sekund (1 minuta)
- Výchozí: 300 sekund (5 minut)
- Zvyšte pokud vidíte časté přepínání režimů

#### `average_household_load_kw` (povinné, desetinné číslo)

- Průměrná spotřeba domácnosti v kilowattech
- Používá se jako záloha když skutečný senzor zatížení není dostupný
- Typické hodnoty:
  - Malá domácnost: 0,3-0,5 kW
  - Střední domácnost: 0,5-0,8 kW
  - Velká domácnost: 0,8-1,5 kW
- Výchozí: `0.5` kW (500W)
- FluxION preferuje skutečný senzor zatížení pokud je dostupný

#### `min_consecutive_force_blocks` (povinné, celé číslo)

- Minimální počet po sobě jdoucích 15minutových bloků pro vynucené operace
- Zabraňuje jednoblokovým vynuceným operacím způsobujícím nadměrné zápisy do střídače
- Hodnoty:
  - `1` = povolit jednotlivé 15minutové bloky (nedoporučeno)
  - `2` = minimum 30 minut (výchozí, doporučeno)
  - `4` = minimum 1 hodina (konzervativnější)
- Výchozí: `2`

## Konfigurace systému

Celkové nastavení systému a připojení k Home Assistant.

```toml
[system]
# Debug režim - loguje akce bez provádění změn v hardware
debug_mode = true

# Interval aktualizace v sekundách (jak často běží řídicí smyčka)
update_interval_secs = 60

# Úroveň logování (error, warn, info, debug, trace)
log_level = "info"

# Zobrazovaná měna pro webové UI (EUR, USD, CZK)
display_currency = "CZK"

# Jazyk uživatelského rozhraní (en, cs)
language = "cs"

# Připojení k Home Assistant (volitelné - auto-detekováno v režimu doplňku)
# ha_base_url = "http://homeassistant.local:8123"
# ha_token = "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
```

### Konfigurační pole

#### `debug_mode` (povinné, boolean)

- Bezpečný režim, který loguje akce bez provádění změn v hardware
- **Výchozí: `true` pro bezpečnost**
- Hodnoty:
  - `true` - Simulovat řídicí akce, logovat co by se stalo (BEZPEČNÉ)
  - `false` - Provádět skutečné změny v nastavení střídače (PRODUKČNÍ)
- **Důležité**: Vždy nejprve testujte s `debug_mode = true`
- Nastavte na `false` pouze po:
  - Ověření konfigurace
  - Testování v debug režimu
  - Pochopení řídicích strategií

#### `update_interval_secs` (povinné, celé číslo)

- Jak často FluxION spouští svou řídicí smyčku (v sekundách)
- Minimum: 10 sekund
- Doporučeno: 60 sekund (1 minuta)
- Typické hodnoty:
  - `60` - dobrá rovnováha odezvy a zatížení
  - `30` - rychlejší odezva, vyšší zatížení systému
  - `120` - nižší zatížení, pomalejší reakce na změny
- Vyšší hodnoty:
  - Snižují zatížení systému
  - Používají méně síťové šířky pásma
  - Pomalejší reakce na změny cen/podmínek

#### `log_level` (povinné, řetězec)

- Řídí podrobnost výstupu logování
- Hodnoty (od nejméně po nejvíce podrobné):
  - `"error"` - Pouze kritické chyby
  - `"warn"` - Chyby a varování
  - `"info"` - Informace o normálním provozu (doporučeno)
  - `"debug"` - Detailní ladicí informace
  - `"trace"` - Extrémně podrobné (pouze pro vývoj)
- Použijte `"debug"` pro odstraňování problémů
- Použijte `"info"` pro normální provoz

#### `display_currency` (povinné, řetězec)

- Měna používaná ve webovém UI a logech
- Hodnoty:
  - `"EUR"` - Euro
  - `"USD"` - Americké dolary
  - `"CZK"` - České koruny
- Ovlivňuje pouze zobrazení, ne výpočty
- Přizpůsobte měně vašeho senzoru spotové ceny

#### `language` (povinné, řetězec)

- Jazyk uživatelského rozhraní
- Podporované hodnoty:
  - `"en"` - Angličtina
  - `"cs"` - Čeština
- Výchozí: `"en"`
- Ovlivňuje webové UI, logy a zprávy

#### `ha_base_url` (volitelné, řetězec)

- Základní URL Home Assistant
- **Potřebné pouze při spuštění mimo doplněk Home Assistant**
- Formát: `"http://hostname:port"` nebo `"https://hostname:port"`
- Příklady:
  - `"http://homeassistant.local:8123"`
  - `"http://192.168.1.100:8123"`
  - `"https://ha.example.com"`
- Není potřeba v režimu doplňku (auto-detekce)

#### `ha_token` (volitelné, řetězec)

- Dlouhodobý přístupový token Home Assistant
- **Potřebné pouze při spuštění mimo doplněk Home Assistant**
- Získání tokenu z HA:
  1. Přihlaste se do Home Assistant
  2. Klikněte na svůj profil (vlevo dole)
  3. Sjeďte dolů na "Dlouhodobé přístupové tokeny"
  4. Klikněte "Vytvořit token"
  5. Zkopírujte token
- Bezpečnostní varování: Nikdy necommitujte tokeny do gitu
- Není potřeba v režimu doplňku (automaticky používá `SUPERVISOR_TOKEN`)

#### `timezone` (volitelné, řetězec, auto-detekováno)

- Systémové časové pásmo (např. "Europe/Prague")
- **Auto-detekováno z Home Assistant při startu**
- Obvykle se nekonfiguruje ručně
- Používá se pro plánování a časově závislé strategie

## Konfigurace strategií

Jemné doladění optimalizačních strategií.

```toml
[strategies.winter_peak_discharge]
# Povolit zimní strategii vybíjení při špičce
enabled = true

# Minimální cenový rozdíl potřebný pro spuštění vybíjení (měna/kWh)
min_spread_czk = 3.0

# Minimální SOC potřebný pro zahájení vybíjení (%)
min_soc_to_start = 70.0

# Cílové minimální SOC po vybití (%)
min_soc_target = 50.0

# Začátek okna solární produkce - hodina (0-23)
solar_window_start_hour = 9

# Konec okna solární produkce - hodina (0-23)
solar_window_end_hour = 15

# Minimální počet hodin před solárním oknem pro povolení vybíjení
min_hours_to_solar = 4

[strategies.solar_aware_charging]
# Povolit strategii nabíjení s ohledem na solární produkci
enabled = true

# Začátek okna solární produkce - hodina (0-23)
solar_window_start_hour = 9

# Konec okna solární produkce - hodina (0-23)
solar_window_end_hour = 12

# Maximální SOC v poledne pro ponechání prostoru pro solární energii (%)
midday_max_soc = 90.0

# Minimální solární prognóza pro aktivaci této strategie (kWh)
min_solar_forecast_kwh = 2.0

[strategies.seasonal]
# Vynutit konkrétní roční období (volitelné)
# force_season = "winter"  # Možnosti: "winter", "summer"
```

### Strategie zimního vybíjení při špičce

Optimalizuje vybíjení baterie během drahých večerních špiček při zachování dostatečného nabití na
noc.

#### `enabled` (boolean, výchozí: true)

- Povolit/zakázat tuto strategii
- `true` - Strategie se účastní rozhodování
- `false` - Strategie je neaktivní

#### `min_spread_czk` (desetinné číslo, výchozí: 3.0)

- Minimální cenový rozdíl (měna/kWh) mezi špičkou a průměrem pro spuštění vybíjení
- Vyšší hodnoty = konzervativnější (vybíjet pouze při velkých cenových skocích)
- Nižší hodnoty = agresivnější (vybíjet při menších rozdílech)
- Zabraňuje vybíjení když je arbitrážní zisk příliš malý

#### `min_soc_to_start` (desetinné číslo, výchozí: 70.0, rozsah: 0-100)

- Minimální SOC baterie (%) potřebný pro zahájení špičkového vybíjení
- Strategie nebude vybíjet pokud je baterie pod touto úrovní
- Zajišťuje dostatečné nabití před agresivním vybíjením

#### `min_soc_target` (desetinné číslo, výchozí: 50.0, rozsah: 0-100)

- Cílové minimální SOC (%) po dokončení vybíjení
- Strategie se snaží udržet SOC nad touto hodnotou během vybíjení
- Mělo by být menší než `min_soc_to_start`

#### `solar_window_start_hour` (celé číslo, výchozí: 9, rozsah: 0-23)

- Hodina kdy obvykle začína solární produkce
- Strategie se vyhýbá vybíjení příliš blízko solárních hodin

#### `solar_window_end_hour` (celé číslo, výchozí: 15, rozsah: 0-23)

- Hodina kdy obvykle končí významná solární produkce
- Definuje okno solární produkce

#### `min_hours_to_solar` (celé číslo, výchozí: 4)

- Minimální počet hodin před solárním oknem potřebný pro povolení vybíjení
- Zabraňuje vyčerpání baterie těsně před začátkem solární produkce
- Příklad: pokud solár začíná v 9:00 a toto je 4, žádné vybíjení po 5:00

### Strategie nabíjení s ohledem na solární produkci

Vyhýbá se nabíjení baterie ze sítě těsně před očekávanou solární produkcí, nechává prostor pro
zachycení solární energie.

#### `enabled` (boolean, výchozí: true)

- Povolit/zakázat tuto strategii

#### `solar_window_start_hour` (celé číslo, výchozí: 9, rozsah: 0-23)

- Hodina kdy obvykle začíná solární produkce
- Strategie se vyhýbá nabíjení ze sítě krátce před tímto časem

#### `solar_window_end_hour` (celé číslo, výchozí: 12, rozsah: 0-23)

- Hodina kdy obvykle končí špičková solární produkce
- Definuje ranní solární okno

#### `midday_max_soc` (desetinné číslo, výchozí: 90.0, rozsah: 0-100)

- Maximální cílové SOC (%) před solárním oknem
- Nechává prostor pro absorbci solární produkce
- Příklad: `90.0` = ponechat 10% kapacity pro solár

#### `min_solar_forecast_kwh` (desetinné číslo, výchozí: 2.0)

- Minimální solární prognóza (kWh) potřebná pro aktivaci této strategie
- Zabraňuje zbytečnému omezování nabíjení když se očekává málo sluníčka
- Nižší hodnoty = konzervativnější (aktivovat častěji)
- Vyšší hodnoty = aktivovat pouze když se očekává významná solární produkce

### Konfigurace ročních období

#### `force_season` (volitelné, řetězec)

- Přepsat automatickou detekci ročního období
- Hodnoty:
  - `"winter"` - Vynutit zimní strategie
  - `"summer"` - Vynutit letní strategie
  - Nenastaveno nebo prázdné - Auto-detekce období
- Užitečné pro testování nebo regiony s neobvyklými ročními obdobími

## Proměnné prostředí

Přepsání konfiguračních hodnot pomocí proměnných prostředí (užitečné pro vývoj, testování nebo
Docker nasazení).

### Dostupné proměnné

```bash
# ID entity senzoru spotové ceny
export SPOT_PRICE_ENTITY="sensor.custom_spot_price"

# Přepsání debug režimu
export DEBUG_MODE=true    # nebo false

# Přepsání intervalu aktualizace (sekundy)
export UPDATE_INTERVAL_SECS=60

# Připojení k Home Assistant
export HA_BASE_URL="http://homeassistant.local:8123"
export HA_TOKEN="eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9..."
```

### Pořadí priority

Když je stejné nastavení definováno na více místech:

1. **Proměnné prostředí** (nejvyšší priorita)
2. **Konfigurační soubor** (config.toml, config.json, nebo /data/options.json)
3. **Výchozí hodnoty** (nejnižší priorita)

### Příklad použití

```bash
# Spustit s vypnutým debug režimem přes proměnnou prostředí
# (i když config.toml má debug_mode = true)
export DEBUG_MODE=false
cargo run --release

# Spustit s vlastním HA připojením
export HA_BASE_URL="http://192.168.1.100:8123"
export HA_TOKEN="your_token_here"
cargo run --release
```

## Kompletní příklady

### Příklad 1: Jeden střídač Solax se spotovými cenami

Typická česká domácnost s jedním střídačem a optimalizací spotových cen.

```toml
[[inverters]]
id = "solax_main"
vendor = "solax"
entity_prefix = "solax"
topology = "independent"

[pricing]
spot_price_entity = "sensor.current_spot_electricity_price_15min"
use_spot_prices_to_buy = true
use_spot_prices_to_sell = true
fixed_buy_prices = [0.05; 24]
fixed_sell_prices = [0.08; 24]

[control]
maximum_export_power_w = 5000
force_charge_hours = 4
force_discharge_hours = 2
min_battery_soc = 15.0
max_battery_soc = 100.0
hardware_min_battery_soc = 10.0
battery_capacity_kwh = 23.0
battery_wear_cost_czk_per_kwh = 0.125
battery_efficiency = 0.95
min_mode_change_interval_secs = 300
average_household_load_kw = 0.5
min_consecutive_force_blocks = 2

[strategies.winter_peak_discharge]
enabled = true
min_spread_czk = 3.0
min_soc_to_start = 70.0
min_soc_target = 50.0
solar_window_start_hour = 9
solar_window_end_hour = 15
min_hours_to_solar = 4

[strategies.solar_aware_charging]
enabled = true
solar_window_start_hour = 9
solar_window_end_hour = 12
midday_max_soc = 90.0
min_solar_forecast_kwh = 2.0

[system]
debug_mode = true
update_interval_secs = 60
log_level = "info"
display_currency = "CZK"
language = "cs"
```

### Příklad 2: Více střídačů v konfiguraci Master/Slave

Tři střídače Solax v master-slave konfiguraci pro větší instalaci.

```toml
[[inverters]]
id = "master"
vendor = "solax"
entity_prefix = "solax_master"
topology = "master"
slaves = ["slave_1", "slave_2"]

[[inverters]]
id = "slave_1"
vendor = "solax"
entity_prefix = "solax_s1"
topology = "slave"
master = "master"

[[inverters]]
id = "slave_2"
vendor = "solax"
entity_prefix = "solax_s2"
topology = "slave"
master = "master"

[pricing]
spot_price_entity = "sensor.spot_price"
use_spot_prices_to_buy = true
use_spot_prices_to_sell = true
fixed_buy_prices = [0.05; 24]
fixed_sell_prices = [0.08; 24]

[control]
maximum_export_power_w = 15000    # Vyšší pro více střídačů
force_charge_hours = 6
force_discharge_hours = 3
min_battery_soc = 15.0
max_battery_soc = 95.0
hardware_min_battery_soc = 10.0
battery_capacity_kwh = 69.0        # 3x 23 kWh baterie
battery_wear_cost_czk_per_kwh = 0.125
battery_efficiency = 0.95
min_mode_change_interval_secs = 300
average_household_load_kw = 1.2    # Větší domácnost
min_consecutive_force_blocks = 2

[strategies.winter_peak_discharge]
enabled = true
min_spread_czk = 2.5
min_soc_to_start = 70.0
min_soc_target = 50.0
solar_window_start_hour = 9
solar_window_end_hour = 15
min_hours_to_solar = 4

[strategies.solar_aware_charging]
enabled = true
solar_window_start_hour = 9
solar_window_end_hour = 12
midday_max_soc = 85.0              # Konzervativnější
min_solar_forecast_kwh = 5.0       # Vyšší práh

[system]
debug_mode = false                 # Produkční režim
update_interval_secs = 60
log_level = "info"
display_currency = "EUR"
language = "cs"
```

### Příklad 3: Fixní ceny (bez spotového trhu)

Pro regiony bez spotových cen nebo se smlouvami s fixní sazbou.

```toml
[[inverters]]
id = "main"
vendor = "solax"
entity_prefix = "solax"
topology = "independent"

[pricing]
spot_price_entity = "sensor.spot_price"  # Stále potřebné ale ignorováno
use_spot_prices_to_buy = false           # Vypnout spotové ceny
use_spot_prices_to_sell = false

# Časové pásmo cen (příklad pro český tarif ČEZ D57d)
fixed_buy_prices = [
    # Nízká sazba: 00:00-08:00
    1.20, 1.20, 1.20, 1.20, 1.20, 1.20, 1.20, 1.20,
    # Vysoká sazba: 08:00-20:00
    2.50, 2.50, 2.50, 2.50, 2.50, 2.50, 2.50, 2.50, 2.50, 2.50, 2.50, 2.50,
    # Nízká sazba: 20:00-00:00
    1.20, 1.20, 1.20, 1.20
]

fixed_sell_prices = [
    # Výkupní cena (konstantní po celý den)
    1.50, 1.50, 1.50, 1.50, 1.50, 1.50, 1.50, 1.50,
    1.50, 1.50, 1.50, 1.50, 1.50, 1.50, 1.50, 1.50,
    1.50, 1.50, 1.50, 1.50, 1.50, 1.50, 1.50, 1.50
]

[control]
maximum_export_power_w = 5000
force_charge_hours = 8              # Nabíjet během období nízké sazby
force_discharge_hours = 12          # Vybíjet během období vysoké sazby
min_battery_soc = 20.0
max_battery_soc = 100.0
hardware_min_battery_soc = 10.0
battery_capacity_kwh = 23.0
battery_wear_cost_czk_per_kwh = 0.125
battery_efficiency = 0.95
min_mode_change_interval_secs = 300
average_household_load_kw = 0.6
min_consecutive_force_blocks = 4    # Delší bloky pro TOU

[strategies.winter_peak_discharge]
enabled = false                     # Není užitečné bez cenové volatility

[strategies.solar_aware_charging]
enabled = true                      # Stále užitečné pro solární optimalizaci
solar_window_start_hour = 9
solar_window_end_hour = 12
midday_max_soc = 90.0
min_solar_forecast_kwh = 1.5

[system]
debug_mode = true
update_interval_secs = 60
log_level = "info"
display_currency = "CZK"
language = "cs"
```

## Validační pravidla

FluxION validuje konfiguraci při startu. Běžné validační chyby:

### Validace střídačů

- Alespoň jeden střídač musí být nakonfigurován
- Každý střídač musí mít unikátní `id`
- `entity_prefix` nemůže být prázdný
- Topologie musí být: `independent`, `master`, nebo `slave`
- Master střídače musí uvádět alespoň jednoho slave v `slaves`
- Slave střídače musí odkazovat na platný master v `master`

### Validace cen

- `spot_price_entity` nemůže být prázdný
- `fixed_buy_prices` musí mít přesně 24 nebo 96 hodnot
- `fixed_sell_prices` musí mít přesně 24 nebo 96 hodnot
- Všechny ceny musí být nezáporné

### Validace řízení

- `min_battery_soc` musí být mezi 0 a 100
- `max_battery_soc` musí být mezi 0 a 100
- `min_battery_soc` < `max_battery_soc`
- `min_battery_soc` >= `hardware_min_battery_soc`
- `hardware_min_battery_soc` musí být mezi 0 a 100
- `battery_capacity_kwh` musí být kladné
- `battery_wear_cost_czk_per_kwh` musí být nezáporné
- `battery_efficiency` musí být mezi 0.0 a 1.0
- `min_mode_change_interval_secs` musí být >= 60 sekund
- `min_consecutive_force_blocks` musí být >= 1

### Validace systému

- `update_interval_secs` musí být >= 10 sekund
- `log_level` musí být jedna z: error, warn, info, debug, trace
- `display_currency` by měla být jedna z: EUR, USD, CZK
- `language` by měl být jedna z: en, cs

## Řešení problémů

### Konfigurační soubor nenalezen

**Chyba**: "No configuration file found, using defaults"

**Řešení**:

1. Vytvořte `config.toml` v pracovním adresáři
2. Zkopírujte z `config.example.toml`
3. Nebo nastavte konfiguraci přes proměnné prostředí

### Selhání parsování konfigurace

**Chyba**: "Failed to parse config.toml: ..."

**Řešení**:

1. Zkontrolujte syntaxi TOML (použijte TOML validátor)
2. Ujistěte se, že všechna povinná pole jsou přítomna
3. Zkontrolujte chybějící čárky v polích
4. Ujistěte se o uvozovkách kolem řetězců
5. Validujte formáty čísel (žádné koncové čárky)

### Chyby konfigurace střídačů

**Chyba**: "Configuration must include at least one inverter"

- Přidejte alespoň jednu sekci `[[inverters]]`

**Chyba**: "Inverter 'X' is configured as master but has no slaves"

- Přidejte `slaves = ["slave_id"]` do master konfigurace

**Chyba**: "Inverter 'X' is configured as slave but has no master"

- Přidejte `master = "master_id"` do slave konfigurace

### Chyby konfigurace cen

**Chyba**: "fixed_buy_prices must have 24 or 96 values, got X"

- Ujistěte se, že cenová pole mají přesně 24 (hodinové) nebo 96 (15-min) hodnot
- Pečlivě spočítejte prvky pole
- Každá hodina potřebuje jednu hodnotu pro 24hodinový formát

### Chyby SOC baterie

**Chyba**: "min_battery_soc must be less than max_battery_soc"

- Zkontrolujte, že `min_battery_soc` < `max_battery_soc`
- Příklad: min=15.0, max=100.0 (správně)
- Příklad: min=80.0, max=70.0 (nesprávně)

**Chyba**: "min_battery_soc must be between 0 and 100"

- Ujistěte se, že hodnoty SOC jsou procenta (0-100)
- Nepoužívejte desetinný zápis jako 0.15 pro 15%

### Chyby připojení k Home Assistant

**Chyba**: "Failed to connect to Home Assistant"

**Řešení**:

1. Ověřte, že `ha_base_url` je správné a dostupné
2. Zkontrolujte, že `ha_token` je platný a není vypršelý
3. Ujistěte se, že Home Assistant běží
4. Ověřte síťové připojení
5. Zkontrolujte nastavení firewallu
6. Pro režim doplňku: ujistěte se, že běží v prostředí HA doplňku

### Chyby konfigurace strategií

**Varování**: "force_charge_hours is 0 - no charging will be scheduled"

- Toto je záměrné pokud chcete pouze solární nabíjení
- Nastavte na nenulovou hodnotu pro nabíjení ze sítě

**Varování**: "force_discharge_hours is 0 - no discharging will be scheduled"

- Toto je záměrné pokud chcete pouze vlastní spotřebu
- Nastavte na nenulovou hodnotu pro arbitrážní vybíjení

## Získání pomoci

### Dlouhodobý přístupový token Home Assistant

Pro vývoj mimo HA doplněk:

1. Přihlaste se do webového rozhraní Home Assistant
2. Klikněte na ikonu svého profilu (vlevo dole)
3. Sjeďte dolů na sekci "Dlouhodobé přístupové tokeny"
4. Klikněte "Vytvořit token"
5. Dejte mu popisný název (např. "FluxION Vývoj")
6. Okamžitě zkopírujte token (zobrazí se pouze jednou)
7. Přidejte do `config.toml`:
   ```toml
   [system]
   ha_token = "VÁŠ_TOKEN_ZDE"
   ```

**Bezpečnostní varování**: Uchovávejte tokeny v tajnosti! Přidejte `config.toml` do `.gitignore`
pokud obsahuje tokeny.

### Kontrola konfigurace

Použijte vestavěnou validaci ve FluxION:

```bash
# Spusťte FluxION - validuje konfiguraci při startu
cargo run --release

# Zkontrolujte logy pro validační chyby
# Hledejte řádky jako:
# ✅ Loaded configuration from config.toml
# ❌ Configuration validation failed: ...
```

### Zdroje podpory

- **Dokumentace**: Viz adresář `/fluxion/docs/`
- **Průvodce konfigurací**: Tento soubor
- **Průvodce nasazením**: `docs/guides/DEPLOYMENT.md`
- **Architektura**: `docs/architecture/ARCHITECTURE.md`
- **Sledování problémů**: https://github.com/SolarE-cz/fluxion/issues
- **Komerční podpora**: info@solare.cz

______________________________________________________________________

**Poslední aktualizace**: 2025-10-31 **Verze FluxION**: 0.1.0 (MVP)
