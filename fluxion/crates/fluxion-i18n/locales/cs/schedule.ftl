# Důvody plánu
reason-cheapest-block = Nejlevnější blok ({ $price } { $currency }/kWh)
reason-peak-price = Špičková cena ({ $price } { $currency }/kWh)
reason-normal-operation = Normální provoz ({ $price } { $currency }/kWh)
reason-forced-charge = Nucené nabíjení (přepsáno uživatelem)
reason-forced-discharge = Nucené vybíjení (přepsáno uživatelem)
reason-backup-reserve = Údržba záložní rezervy
reason-grid-limit = Limit dodávky do sítě
reason-battery-protection = Ochrana baterie
reason-temperature-limit = Teplotní limit
reason-manual-mode = Manuální režim

# Stavy plánu
state-charging = Nabíjení
state-discharging = Vybíjení
state-idle = Nečinný
state-self-use = Vlastní spotřeba

# Časové údaje
time-now = Nyní
time-next = Další
time-in = Za { $minutes ->
    [one] { $minutes } minutu
    [few] { $minutes } minuty
   *[other] { $minutes } minut
}
time-until = Do { $time }
time-from-to = Od { $start } do { $end }

# Informace o bloku
block-duration = Trvání: { $hours ->
    [one] { $hours } hodina
    [few] { $hours } hodiny
   *[other] { $hours } hodin
}
block-energy = Energie: { $energy } kWh
block-savings = Odhadovaná úspora: { $amount } { $currency }

# Důvody zimní strategie
reason-winter-peak-discharge = Zimní špičkové vybíjení: cena { $price } { $currency }/kWh (rozpětí { $spread }), cíl ≥ { $target_soc }% ({ $hours_to_solar }h do solárního okna)
reason-solar-aware-charge = Nabíjení s ohledem na solár: cíl { $target_soc }% ({ $hours_to_solar }h do solárního okna, předpověď { $forecast } kWh)
reason-solar-aware-charge-marginal = Nabíjení s ohledem na solár (marginální): cíl { $target_soc }% za { $price } { $currency }/kWh (průměr { $avg_price })
reason-winter-soc-below-start = SOC { $soc }% pod startovací hranicí { $min }%
reason-winter-near-solar-window = Blízko solárního okna ({ $start }-{ $end }h), přeskakuji vybíjení
reason-winter-low-solar-hours = Pouze { $hours_to_solar }h do solárního okna a nízká předpověď { $forecast } kWh
reason-winter-spread-too-low = Rozpětí { $spread } { $currency } < min { $min } { $currency }
reason-winter-soc-at-target = Aktuální SOC { $soc }% <= bezpečný cíl { $target }%
reason-winter-not-profitable = Neziskové po nákladech za cenu { $price } { $currency }/kWh
reason-solar-aware-soc-reached = Aktuální SOC { $soc }% >= cíl { $target }%
reason-solar-aware-price-high = Cena { $price } > 1.2×průměr { $avg }
