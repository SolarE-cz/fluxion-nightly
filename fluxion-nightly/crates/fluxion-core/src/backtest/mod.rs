// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! Backtest of one past day on the site's own Home Assistant history (brief H28): the day
//! loader, the block-by-block Winter Reserve replay and the day report the web page shows.

mod day;
mod replay;
mod report;

pub use day::{
    AvailableDay, BLOCK_MINUTES, BacktestEntities, BacktestSite, FORECAST_HISTORY_DAYS, Field,
    HistoryError, HistoryReader, HistoryState, LoadError, LoadedBlock, LoadedDay, SiteHistory,
    Timeline, apply_planner_prices, available_days, build_blocks, date_in_range, day_block_starts,
    earliest_backtest_date, hdo_schedules_from_history, load_day, local_midnight_utc,
};
pub use replay::{
    DayTotals, MODE_LETTERS, ProfileSource, ReplayOptions, ReplayRow, ReplayRun, RunTotals,
    SimulatedBlock, StrategyStep, is_night, mode_index, replay,
};
pub use report::{
    DataQuality, DayReport, FeesUsed, FloorUsed, StrategySeries, StrategyTotals, build_report,
    winter_reserve_params,
};

#[cfg(test)]
mod tests;
