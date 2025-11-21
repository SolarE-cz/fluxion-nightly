use chrono::NaiveDate;
use fluxion_core::ote_market_data::OteMarketData;
use std::fs::File;
use std::io::Write;

fn main() -> anyhow::Result<()> {
    // Set up logging
    tracing_subscriber::fmt::init();

    let fetcher = OteMarketData::new();
    
    //  Download full October 2025
    let start_date = NaiveDate::from_ymd_opt(2025, 10, 1).unwrap();
    let end_date = NaiveDate::from_ymd_opt(2025, 10, 31).unwrap();
    
    println!("Downloading OTE data for October 2025 ({} to {})...", start_date, end_date);
    let records = fetcher.fetch_range(start_date, end_date)?;
    
    println!("Fetched {} price records total", records.len());
    
    // Save to CSV
    let output_path = "data/prices_2025_10.csv";
    std::fs::create_dir_all("data")?;
    let mut file = File::create(output_path)?;
    writeln!(file, "datetime,price_eur,price_czk")?;
    
    for record in &records {
        writeln!(file, "{},{},{}", 
            record.datetime.format("%Y-%m-%d %H:%M:%S"),
            record.price_eur,
            record.price_czk)?;
    }
    
    println!("Saved {} records to {}", records.len(), output_path);
    
    // Print first few and last few to verify
    println!("\nFirst 5 records:");
    for record in records.iter().take(5) {
        println!("  {} - EUR: {:.2}, CZK: {:.2}", 
            record.datetime, record.price_eur, record.price_czk);
    }
    
    println!("\nLast 5 records:");
    for record in records.iter().rev().take(5).rev() {
        println!("  {} - EUR: {:.2}, CZK: {:.2}", 
            record.datetime, record.price_eur, record.price_czk);
    }

    Ok(())
}
