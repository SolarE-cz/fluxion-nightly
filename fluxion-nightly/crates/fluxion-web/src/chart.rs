// Copyright (c) 2025 SOLARE S.R.O.
//
// This file is part of FluxION.
//
// Licensed under the Creative Commons Attribution-NonCommercial-NoDerivatives 4.0 International
// (CC BY-NC-ND 4.0). You may use and share this file for non-commercial purposes only and you may not
// create derivatives. See <https://creativecommons.org/licenses/by-nc-nd/4.0/>.
//
// This software is provided "AS IS", without warranty of any kind.
//
// For commercial licensing, please contact: info@solare.cz

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use plotters::prelude::*;

/// Generate SVG chart for electricity prices
///
/// # Errors
/// Returns error if chart generation fails
///
/// # Panics
/// Panics if blocks array is empty
pub fn generate_price_chart_svg(
    blocks: &[(DateTime<Utc>, f32, String)], // (timestamp, price, mode)
    width: u32,
    height: u32,
    timezone: Option<&str>,
) -> Result<String, Box<dyn std::error::Error>> {
    let mut svg_data = String::new();

    if blocks.is_empty() {
        return Ok(svg_data);
    }

    {
        let root = SVGBackend::with_string(&mut svg_data, (width, height)).into_drawing_area();
        root.fill(&RGBColor(26, 26, 26))?; // Dark background

        // Find price range
        let prices: Vec<f32> = blocks.iter().map(|(_, p, _)| *p).collect();
        let min_price = prices.iter().copied().fold(f32::INFINITY, f32::min);
        let max_price = prices.iter().copied().fold(f32::NEG_INFINITY, f32::max);

        // Add 10% padding to price range
        let price_range = max_price - min_price;
        let y_min = if min_price < 0.0 {
            min_price - price_range * 0.1
        } else {
            0.0
        };
        let y_max = max_price + price_range * 0.1;

        let first_time = blocks.first().unwrap().0;
        let last_time = blocks.last().unwrap().0;

        // Calculate actual hours of data
        #[expect(
            clippy::cast_precision_loss,
            reason = "block count will never exceed mantissa precision"
        )]
        let hours = blocks.len() as f32 / 4.0;
        let caption = format!("Electricity Price ({hours:.0}h schedule)");

        let mut chart = ChartBuilder::on(&root)
            .caption(&caption, ("sans-serif", 20, &WHITE))
            .margin(15)
            .x_label_area_size(40)
            .y_label_area_size(60)
            .build_cartesian_2d(first_time..last_time, y_min..y_max)?;

        // Configure chart styling with timezone-aware formatting
        chart
            .configure_mesh()
            .x_desc("Time")
            .y_desc("Price (Kč/kWh)")
            .x_labels(12)
            .y_labels(10)
            .x_label_formatter(&|dt| {
                if let Some(tz_name) = timezone
                    && let Ok(tz) = tz_name.parse::<Tz>()
                {
                    return dt.with_timezone(&tz).format("%m-%d %H:%M").to_string();
                }
                dt.format("%m-%d %H:%M").to_string()
            })
            .label_style(("sans-serif", 12, &RGBColor(153, 153, 153)))
            .axis_style(RGBColor(58, 58, 58))
            .draw()?;

        // Draw bars colored by mode
        for (time, price, mode) in blocks {
            let color = match mode.as_str() {
                "charge" => RGBColor(255, 152, 0),    // Orange
                "discharge" => RGBColor(76, 175, 80), // Green
                _ => RGBColor(33, 150, 243),          // Blue
            };

            let bar_width = chrono::Duration::minutes(15);
            let x1 = *time;
            let x2 = *time + bar_width;

            chart.draw_series(std::iter::once(Rectangle::new(
                [(x1, 0.0), (x2, *price)],
                color.filled(),
            )))?;
        }

        root.present()?;
    } // root is dropped here, releasing the borrow on svg_data

    // Add legend to the SVG
    let legend_y = 30;
    let legend_x_start = 950;
    let box_size = 12;
    let spacing = 100;

    let legend_svg = format!(
        "\n<!-- Legend -->\n\
<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"#FF9800\" stroke=\"none\"/>\n\
<text x=\"{}\" y=\"{}\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#e0e0e0\">Charge</text>\n\
<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"#4CAF50\" stroke=\"none\"/>\n\
<text x=\"{}\" y=\"{}\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#e0e0e0\">Discharge</text>\n\
<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\" fill=\"#2196F3\" stroke=\"none\"/>\n\
<text x=\"{}\" y=\"{}\" font-family=\"sans-serif\" font-size=\"12\" fill=\"#e0e0e0\">Self-use</text>\n",
        legend_x_start,
        legend_y,
        box_size,
        box_size,
        legend_x_start + box_size + 5,
        legend_y + box_size - 1,
        legend_x_start + spacing,
        legend_y,
        box_size,
        box_size,
        legend_x_start + spacing + box_size + 5,
        legend_y + box_size - 1,
        legend_x_start + spacing * 2,
        legend_y,
        box_size,
        box_size,
        legend_x_start + spacing * 2 + box_size + 5,
        legend_y + box_size - 1,
    );
    svg_data.insert_str(svg_data.len() - 6, &legend_svg);

    Ok(svg_data)
}
