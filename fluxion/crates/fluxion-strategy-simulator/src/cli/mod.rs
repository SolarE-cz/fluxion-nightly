// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.

//! CLI module for strategy simulator command-line interface.

pub mod args;
pub mod config;
pub mod data_loaders;
pub mod formatters;

pub use args::{BatchArgs, Cli, Commands, CompareArgs, RunArgs};
pub use config::BatchConfig;
pub use data_loaders::{DataLoader, JsonExportLoader, SqliteLoader, SyntheticLoader};
pub use formatters::{CsvFormatter, TableFormatter};
