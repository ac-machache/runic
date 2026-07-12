//! `weather` + `weather_history` — current/forecast and historical conditions
//! via [Open-Meteo].
//!
//! Keyless and accurate: Open-Meteo serves national weather-service models
//! (ICON / GFS / ECMWF) and ERA5 reanalysis, no API key, generous limits. Both
//! tools are coordinate-based, so each is a **two-call flow**: geocode the
//! `location` name → coordinates (the free geocoding endpoint), then fetch.
//!
//! - `weather`: current conditions + a 7-day forecast (`api.open-meteo.com`).
//! - `weather_history`: daily conditions over a past date range, back to 1940
//!   (`archive-api.open-meteo.com`).
//!
//! [Open-Meteo]: https://open-meteo.com

use std::time::Duration;

use async_trait::async_trait;
use serde::Deserialize;
use serde::de::DeserializeOwned;

use runic_tool::{Tool, ToolContext, ToolResult};

const GEOCODE_URL: &str = "https://geocoding-api.open-meteo.com/v1/search";
const FORECAST_URL: &str = "https://api.open-meteo.com/v1/forecast";
const ARCHIVE_URL: &str = "https://archive-api.open-meteo.com/v1/archive";
const FORECAST_DAYS: u32 = 7;

const CURRENT_VARS: &str = "temperature_2m,relative_humidity_2m,apparent_temperature,precipitation,weather_code,wind_speed_10m";
const FORECAST_DAILY_VARS: &str = "weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,precipitation_probability_max";
// Archive has no precipitation_probability.
const ARCHIVE_DAILY_VARS: &str =
    "weather_code,temperature_2m_max,temperature_2m_min,precipitation_sum,wind_speed_10m_max";

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .connect_timeout(Duration::from_secs(10))
        .user_agent("runic/0.1 (weather)")
        .build()
        .expect("reqwest client builds with static config")
}

/// GET + JSON, surfacing HTTP errors (Open-Meteo returns `{error, reason}` with
/// a 4xx) as readable in-band errors instead of opaque parse failures.
async fn get_json<T: DeserializeOwned>(
    client: &reqwest::Client,
    url: &str,
    query: &[(&str, &str)],
) -> Result<T, String> {
    let resp = client
        .get(url)
        .query(query)
        .send()
        .await
        .map_err(|e| format!("request failed: {e}"))?;
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "open-meteo HTTP {code}: {}",
            body.chars().take(200).collect::<String>()
        ));
    }
    resp.json::<T>()
        .await
        .map_err(|e| format!("parse failed: {e}"))
}

/// Resolve a place name to its first geocoding match. Shared by both tools.
async fn geocode(client: &reqwest::Client, location: &str) -> Result<GeoResult, String> {
    let geo: GeoResponse = get_json(
        client,
        GEOCODE_URL,
        &[
            ("name", location),
            ("count", "1"),
            ("language", "en"),
            ("format", "json"),
        ],
    )
    .await?;
    geo.results
        .into_iter()
        .next()
        .ok_or_else(|| format!("could not find a location named '{location}'"))
}

// ── response DTOs (forecast + archive share the daily shape) ────────────────

#[derive(Deserialize)]
struct GeoResponse {
    #[serde(default)]
    results: Vec<GeoResult>,
}

#[derive(Deserialize, Clone)]
struct GeoResult {
    name: String,
    latitude: f64,
    longitude: f64,
    #[serde(default)]
    country: Option<String>,
    #[serde(default)]
    admin1: Option<String>, // region / state
    #[serde(default)]
    timezone: Option<String>,
}

#[derive(Deserialize)]
struct WeatherResponse {
    #[serde(default)]
    current: Option<Current>,
    #[serde(default)]
    daily: Option<Daily>,
}

#[derive(Deserialize)]
struct Current {
    temperature_2m: Option<f64>,
    relative_humidity_2m: Option<f64>,
    apparent_temperature: Option<f64>,
    precipitation: Option<f64>,
    weather_code: Option<i64>,
    wind_speed_10m: Option<f64>,
}

#[derive(Deserialize)]
struct Daily {
    #[serde(default)]
    time: Vec<String>,
    #[serde(default)]
    weather_code: Vec<i64>,
    #[serde(default)]
    temperature_2m_max: Vec<f64>,
    #[serde(default)]
    temperature_2m_min: Vec<f64>,
    #[serde(default)]
    precipitation_sum: Vec<f64>,
    /// Forecast only — absent for the archive (`#[serde(default)]` → empty).
    #[serde(default)]
    precipitation_probability_max: Vec<Option<f64>>,
}

// ── pure helpers (unit-tested) ──────────────────────────────────────────────

/// WMO weather interpretation code → human text (Open-Meteo's `weather_code`).
fn wmo_text(code: i64) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 => "Fog",
        48 => "Depositing rime fog",
        51 => "Light drizzle",
        53 => "Moderate drizzle",
        55 => "Dense drizzle",
        56 => "Light freezing drizzle",
        57 => "Dense freezing drizzle",
        61 => "Slight rain",
        63 => "Moderate rain",
        65 => "Heavy rain",
        66 => "Light freezing rain",
        67 => "Heavy freezing rain",
        71 => "Slight snowfall",
        73 => "Moderate snowfall",
        75 => "Heavy snowfall",
        77 => "Snow grains",
        80 => "Slight rain showers",
        81 => "Moderate rain showers",
        82 => "Violent rain showers",
        85 => "Slight snow showers",
        86 => "Heavy snow showers",
        95 => "Thunderstorm",
        96 => "Thunderstorm with slight hail",
        99 => "Thunderstorm with heavy hail",
        _ => "Unknown",
    }
}

/// `2026-06-23` → `Tue`, else the raw string.
fn weekday(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%a").to_string())
        .unwrap_or_else(|_| date.to_string())
}

/// `City, Region, Country (TZ)`.
fn place_label(g: &GeoResult) -> String {
    let mut parts = vec![g.name.clone()];
    if let Some(a) = &g.admin1
        && !a.is_empty()
        && a != &g.name
    {
        parts.push(a.clone());
    }
    if let Some(c) = &g.country {
        parts.push(c.clone());
    }
    let mut label = parts.join(", ");
    if let Some(tz) = &g.timezone {
        label.push_str(&format!(" ({tz})"));
    }
    label
}

/// The "Now: …" current-conditions line.
fn render_current(c: &Current, fahrenheit: bool) -> String {
    let t = if fahrenheit { "°F" } else { "°C" };
    let w = if fahrenheit { "mph" } else { "km/h" };
    let f1 = |v: Option<f64>| v.map(|x| format!("{x:.0}")).unwrap_or_else(|| "?".into());
    let cond = c.weather_code.map(wmo_text).unwrap_or("Unknown");
    format!(
        "Now: {}{t} (feels {}{t}), {cond} — humidity {}%, wind {} {w}, precip {} mm",
        f1(c.temperature_2m),
        f1(c.apparent_temperature),
        f1(c.relative_humidity_2m),
        f1(c.wind_speed_10m),
        f1(c.precipitation),
    )
}

/// The indented per-day lines (shared by forecast + history).
fn render_daily(d: &Daily, fahrenheit: bool) -> String {
    let t = if fahrenheit { "°F" } else { "°C" };
    let mut out = String::new();
    for i in 0..d.time.len() {
        let date = &d.time[i];
        let cond = d
            .weather_code
            .get(i)
            .map(|c| wmo_text(*c))
            .unwrap_or("Unknown");
        let max = d
            .temperature_2m_max
            .get(i)
            .map(|x| format!("{x:.0}"))
            .unwrap_or_else(|| "?".into());
        let min = d
            .temperature_2m_min
            .get(i)
            .map(|x| format!("{x:.0}"))
            .unwrap_or_else(|| "?".into());
        let precip = d.precipitation_sum.get(i).copied().unwrap_or(0.0);
        let prob = d
            .precipitation_probability_max
            .get(i)
            .and_then(|p| *p)
            .map(|p| format!(" ({p:.0}%)"))
            .unwrap_or_default();
        out.push_str(&format!(
            "  {} {date}: {cond}, {min}–{max}{t}, precip {precip} mm{prob}\n",
            weekday(date),
        ));
    }
    out.trim_end().to_string()
}

// ── weather (current + forecast) ────────────────────────────────────────────

/// Current conditions + 7-day forecast for a named location.
pub struct WeatherTool {
    client: reqwest::Client,
}

impl Default for WeatherTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherTool {
    pub fn new() -> Self {
        Self { client: client() }
    }
}

#[async_trait]
impl Tool for WeatherTool {
    fn name(&self) -> &str {
        "weather"
    }
    fn description(&self) -> &str {
        "Current conditions and a 7-day forecast for a place. Pass `location` (a \
         city/place name, e.g. \"Paris\" or \"Tokyo, Japan\"); set `units` to \
         \"fahrenheit\" for °F (default celsius)."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "description": "Place name, e.g. 'Paris' or 'Austin, Texas'." },
                "units": { "type": "string", "enum": ["celsius", "fahrenheit"], "description": "Temperature units (default celsius)." }
            },
            "required": ["location"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(location) = args.get("location").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("weather requires `location`"));
        };
        let fahrenheit = args.get("units").and_then(|v| v.as_str()) == Some("fahrenheit");

        let geo = match geocode(&self.client, location).await {
            Ok(g) => g,
            Err(e) => return Ok(ToolResult::error(e)),
        };
        let (lat, lon, days) = (
            geo.latitude.to_string(),
            geo.longitude.to_string(),
            FORECAST_DAYS.to_string(),
        );
        let mut q: Vec<(&str, &str)> = vec![
            ("latitude", &lat),
            ("longitude", &lon),
            ("current", CURRENT_VARS),
            ("daily", FORECAST_DAILY_VARS),
            ("timezone", "auto"),
            ("forecast_days", &days),
        ];
        if fahrenheit {
            q.push(("temperature_unit", "fahrenheit"));
            q.push(("wind_speed_unit", "mph"));
        }
        let fc: WeatherResponse = match get_json(&self.client, FORECAST_URL, &q).await {
            Ok(f) => f,
            Err(e) => return Ok(ToolResult::error(e)),
        };

        let mut out = format!("Weather for {}\n", place_label(&geo));
        if let Some(c) = &fc.current {
            out.push_str(&format!("\n{}\n", render_current(c, fahrenheit)));
        }
        if let Some(d) = &fc.daily {
            out.push_str(&format!("\nForecast:\n{}\n", render_daily(d, fahrenheit)));
        }
        Ok(ToolResult::ok(out.trim_end().to_string()))
    }
}

// ── weather_history (archive) ───────────────────────────────────────────────

/// Daily historical conditions over a past date range (back to 1940).
pub struct WeatherHistoryTool {
    client: reqwest::Client,
}

impl Default for WeatherHistoryTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherHistoryTool {
    pub fn new() -> Self {
        Self { client: client() }
    }
}

#[async_trait]
impl Tool for WeatherHistoryTool {
    fn name(&self) -> &str {
        "weather_history"
    }
    fn description(&self) -> &str {
        "Past daily weather for a place over a date range (reanalysis, back to \
         1940). Pass `location` and `start_date` (YYYY-MM-DD); `end_date` \
         defaults to `start_date` for a single day. `units`: 'fahrenheit' for °F."
    }
    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "location": { "type": "string", "description": "Place name, e.g. 'Paris'." },
                "start_date": { "type": "string", "description": "Start date, YYYY-MM-DD." },
                "end_date": { "type": "string", "description": "End date, YYYY-MM-DD (defaults to start_date)." },
                "units": { "type": "string", "enum": ["celsius", "fahrenheit"], "description": "Temperature units (default celsius)." }
            },
            "required": ["location", "start_date"]
        })
    }
    fn parallelizable(&self) -> bool {
        true
    }
    async fn execute(
        &self,
        args: serde_json::Value,
        _ctx: &ToolContext,
    ) -> anyhow::Result<ToolResult> {
        let Some(location) = args.get("location").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error("weather_history requires `location`"));
        };
        let Some(start) = args.get("start_date").and_then(|v| v.as_str()) else {
            return Ok(ToolResult::error(
                "weather_history requires `start_date` (YYYY-MM-DD)",
            ));
        };
        let end = args
            .get("end_date")
            .and_then(|v| v.as_str())
            .unwrap_or(start);
        let fahrenheit = args.get("units").and_then(|v| v.as_str()) == Some("fahrenheit");

        let geo = match geocode(&self.client, location).await {
            Ok(g) => g,
            Err(e) => return Ok(ToolResult::error(e)),
        };
        let (lat, lon) = (geo.latitude.to_string(), geo.longitude.to_string());
        let mut q: Vec<(&str, &str)> = vec![
            ("latitude", &lat),
            ("longitude", &lon),
            ("start_date", start),
            ("end_date", end),
            ("daily", ARCHIVE_DAILY_VARS),
            ("timezone", "auto"),
        ];
        if fahrenheit {
            q.push(("temperature_unit", "fahrenheit"));
            q.push(("wind_speed_unit", "mph"));
        }
        let arc: WeatherResponse = match get_json(&self.client, ARCHIVE_URL, &q).await {
            Ok(a) => a,
            Err(e) => return Ok(ToolResult::error(e)),
        };

        let mut out = format!(
            "Weather history for {} ({start} → {end})\n",
            place_label(&geo)
        );
        match &arc.daily {
            Some(d) if !d.time.is_empty() => {
                out.push_str(&format!("\n{}\n", render_daily(d, fahrenheit)));
            }
            _ => out.push_str("\n(no data for that range)\n"),
        }
        Ok(ToolResult::ok(out.trim_end().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn geo() -> GeoResult {
        GeoResult {
            name: "Paris".into(),
            latitude: 48.85,
            longitude: 2.35,
            country: Some("France".into()),
            admin1: Some("Île-de-France".into()),
            timezone: Some("Europe/Paris".into()),
        }
    }

    fn current() -> Current {
        Current {
            temperature_2m: Some(14.2),
            relative_humidity_2m: Some(80.0),
            apparent_temperature: Some(12.4),
            precipitation: Some(0.0),
            weather_code: Some(3),
            wind_speed_10m: Some(11.0),
        }
    }

    fn forecast_daily() -> Daily {
        Daily {
            time: vec!["2026-06-23".into(), "2026-06-24".into()],
            weather_code: vec![2, 0],
            temperature_2m_max: vec![18.0, 22.0],
            temperature_2m_min: vec![9.0, 11.0],
            precipitation_sum: vec![2.5, 0.0],
            precipitation_probability_max: vec![Some(40.0), None],
        }
    }

    #[test]
    fn wmo_mapping_covers_the_table() {
        assert_eq!(wmo_text(0), "Clear sky");
        assert_eq!(wmo_text(3), "Overcast");
        assert_eq!(wmo_text(65), "Heavy rain");
        assert_eq!(wmo_text(99), "Thunderstorm with heavy hail");
        assert_eq!(wmo_text(12345), "Unknown");
    }

    #[test]
    fn place_label_joins_and_dedupes() {
        assert_eq!(
            place_label(&geo()),
            "Paris, Île-de-France, France (Europe/Paris)"
        );
    }

    #[test]
    fn weekday_parses_iso_dates() {
        assert_eq!(weekday("2026-06-23"), "Tue");
        assert_eq!(weekday("not-a-date"), "not-a-date");
    }

    #[test]
    fn current_line_renders() {
        let c = render_current(&current(), false);
        assert_eq!(
            c,
            "Now: 14°C (feels 12°C), Overcast — humidity 80%, wind 11 km/h, precip 0 mm"
        );
    }

    #[test]
    fn daily_renders_with_and_without_probability() {
        let r = render_daily(&forecast_daily(), false);
        assert!(r.contains("Tue 2026-06-23: Partly cloudy, 9–18°C, precip 2.5 mm (40%)"));
        // null probability → no percentage
        assert!(r.contains("Wed 2026-06-24: Clear sky, 11–22°C, precip 0 mm"));
        assert!(!r.contains("(0%)"));
    }

    #[test]
    fn fahrenheit_switches_units() {
        let r = render_current(&current(), true);
        assert!(r.contains("°F") && r.contains("mph") && !r.contains("°C"));
    }

    #[test]
    fn archive_daily_without_probability_field_renders() {
        // Archive responses omit precipitation_probability_max entirely.
        let d = Daily {
            time: vec!["1990-07-14".into()],
            weather_code: vec![0],
            temperature_2m_max: vec![31.0],
            temperature_2m_min: vec![18.0],
            precipitation_sum: vec![0.0],
            precipitation_probability_max: vec![], // absent in archive
        };
        let r = render_daily(&d, false);
        assert!(r.contains("1990-07-14: Clear sky, 18–31°C, precip 0 mm"));
        assert!(!r.contains('%'));
    }
}
