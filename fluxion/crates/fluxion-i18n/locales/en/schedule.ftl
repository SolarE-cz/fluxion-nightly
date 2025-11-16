# Schedule Reasons
reason-cheapest-block = Cheapest block ({ $price } { $currency }/kWh)
reason-peak-price = Peak price ({ $price } { $currency }/kWh)
reason-normal-operation = Normal operation ({ $price } { $currency }/kWh)
reason-forced-charge = Forced charge (user override)
reason-forced-discharge = Forced discharge (user override)
reason-backup-reserve = Backup reserve maintenance
reason-grid-limit = Grid export limit
reason-battery-protection = Battery protection
reason-temperature-limit = Temperature limit
reason-manual-mode = Manual mode

# Schedule States
state-charging = Charging
state-discharging = Discharging
state-idle = Idle
state-self-use = Self-consumption

# Time-related
time-now = Now
time-next = Next
time-in = In { $minutes ->
    [one] { $minutes } minute
   *[other] { $minutes } minutes
}
time-until = Until { $time }
time-from-to = From { $start } to { $end }

# Block information
block-duration = Duration: { $hours ->
    [one] { $hours } hour
   *[other] { $hours } hours
}
block-energy = Energy: { $energy } kWh
block-savings = Estimated savings: { $amount } { $currency }

# Winter Strategy Reasons
reason-winter-peak-discharge = Winter peak discharge: price { $price } { $currency }/kWh (spread { $spread }), target ≥ { $target_soc }% ({ $hours_to_solar }h to solar)
reason-solar-aware-charge = Solar-aware charge: target { $target_soc }% ({ $hours_to_solar }h to solar, forecast { $forecast } kWh)
reason-solar-aware-charge-marginal = Solar-aware (marginal): target { $target_soc }% at { $price } { $currency }/kWh (avg { $avg_price })
reason-winter-soc-below-start = SOC { $soc }% below start threshold { $min }%
reason-winter-near-solar-window = Near solar window ({ $start }-{ $end }h), skipping discharge
reason-winter-low-solar-hours = Only { $hours_to_solar }h to solar and low forecast { $forecast } kWh
reason-winter-spread-too-low = Spread { $spread } { $currency } < min { $min } { $currency }
reason-winter-soc-at-target = Current SOC { $soc }% <= safe target { $target }%
reason-winter-not-profitable = Not profitable after costs at price { $price } { $currency }/kWh
reason-solar-aware-soc-reached = Current SOC { $soc }% >= target { $target }%
reason-solar-aware-price-high = Price { $price } > 1.2×avg { $avg }
