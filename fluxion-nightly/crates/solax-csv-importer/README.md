# Solax CSV Importer

A utility to parse Solax inverter CSV export files and import them into a local SQLite database.

## Features

- Parses Solax export CSV files (converted from Excel format)
- Creates a SQLite database with all inverter data fields
- Handles missing/empty values gracefully
- Skips duplicate records based on timestamp
- Creates indexes for efficient querying

## Usage

### Convert Excel to CSV

First, convert the Solax export Excel file to CSV format:

```bash
libreoffice --headless --convert-to csv path/to/H34A10I2293069-YYYY-MM-DD-YYYY-MM-DD.xlsx --outdir /tmp
```

### Import CSV to SQLite

```bash
cargo run --release --bin solax-csv-importer -- \
  --csv /tmp/H34A10I2293069-YYYY-MM-DD-YYYY-MM-DD.csv \
  --database solax_data.db
```

### Options

- `--csv <path>` - Path to the CSV file to import (required)
- `--database <path>` - Path to the SQLite database file (default: `solax_data.db`)

## Database Schema

The utility creates a `solax_data` table with 57 columns capturing all inverter metrics:

- Timestamps and device status
- PV yield (daily and total)
- Battery charge/discharge (daily and total)
- Grid import/export (daily and total)
- Battery management (SOC, temperature, voltage, current)
- MPPT data (power, voltage, current for both channels)
- AC output per phase (L1, L2, L3)
- EPS output per phase
- Inverter temperature

An index is automatically created on `update_time` for efficient time-based queries.

## Example Queries

Once imported, you can query the database:

```sql
-- Get records for a specific date
SELECT * FROM solax_data 
WHERE update_time LIKE '2025-11-01%' 
ORDER BY update_time;

-- Calculate average battery SOC by hour
SELECT 
  strftime('%Y-%m-%d %H', update_time) as hour,
  AVG(total_battery_soc) as avg_soc
FROM solax_data 
GROUP BY hour;

-- Find peak PV power
SELECT update_time, total_pv_power 
FROM solax_data 
ORDER BY total_pv_power DESC 
LIMIT 10;
```

## OTE Price Importer

The package also includes a utility to fetch and import OTE (Czech electricity market) prices into
the same database.

### Usage

```bash
cargo run --release --bin ote-price-importer -- \
  --start-date 2025-11-01 \
  --end-date 2025-11-30 \
  --database solax_data.db
```

### Options

- `--start-date <YYYY-MM-DD>` - Start date for price data (required)
- `--end-date <YYYY-MM-DD>` - End date for price data (required)
- `--database <path>` - Path to the SQLite database file (default: `solax_data.db`)

### Database Schema

The utility creates an `ote_prices` table with:

- `datetime` - Timestamp in RFC3339 format (15-minute intervals)
- `price_eur` - Spot market price in EUR/MWh
- `price_czk` - Spot market price in CZK/MWh

An index is automatically created on `datetime` for efficient time-based queries.

### Example Queries

Join Solax data with OTE prices:

```sql
-- Calculate cost of grid imports
SELECT 
  s.update_time,
  s.grid_power,
  o.price_czk,
  (s.grid_power / 1000.0) * (o.price_czk / 1000.0) as cost_czk
FROM solax_data s
LEFT JOIN ote_prices o 
  ON datetime(s.update_time) = datetime(o.datetime)
WHERE s.grid_power > 0  -- importing from grid
ORDER BY s.update_time;

-- Find times with highest electricity prices
SELECT datetime, price_eur, price_czk
FROM ote_prices
ORDER BY price_eur DESC
LIMIT 20;
```

## Verify Prices

You can verify the imported OTE prices using:

```bash
cargo run --release --bin verify-prices -- --database solax_data.db
```

This will display:

- Total number of price records
- Date range covered
- Sample records (first and last 5)
- Average price for the period

## Notes

- The Solax CSV importer handles empty fields by using default values (0 for numeric fields)
- Duplicate timestamps are automatically skipped for both importers
- The database file is created if it doesn't exist
- OTE prices are fetched directly from the OTE-CR website
