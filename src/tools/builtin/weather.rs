//! Weather tool backed by Open-Meteo.
//!
//! Why Open-Meteo: no API key, free for any use, multi-model forecast
//! ensemble (DWD + NOAA + ECMWF + JMA), simple JSON-over-HTTPS,
//! geocoding included. Adding a free weather provider means no new
//! secret to manage and zero risk of rate-limit-by-key surprises.
//!
//! McKale's default location is Tucson, AZ (lat 32.2226, lon -110.9747)
//! sourced from the vault's `00 System/JARVIS/ObjectiveCharlie.md`
//! address line; an explicit `location` parameter overrides via the
//! Open-Meteo geocoder.
//!
//! Output is shaped for voice. The "current" block is sentence-ready
//! ("it's 85 in Tucson, mostly clear, light wind out of the southwest").
//! Hourly + daily blocks are structured for the LLM to reason about
//! ("will it rain at 3 pm" / "outdoor event Saturday at risk").

use std::time::Instant;

use async_trait::async_trait;
use reqwest::Client;
use serde::Deserialize;

use crate::context::JobContext;
use crate::tools::tool::{Tool, ToolError, ToolOutput};

// Default location: Tucson, AZ. Pulled from the vault's canonical
// McKale address so the tool works with zero parameters out of the box.
const DEFAULT_LAT: f64 = 32.2226;
const DEFAULT_LON: f64 = -110.9747;
const DEFAULT_LOCATION_NAME: &str = "Tucson, AZ";

const FORECAST_BASE: &str = "https://api.open-meteo.com/v1/forecast";
const GEOCODE_BASE: &str = "https://geocoding-api.open-meteo.com/v1/search";

pub struct WeatherTool {
    client: Client,
}

impl Default for WeatherTool {
    fn default() -> Self {
        Self::new()
    }
}

impl WeatherTool {
    pub fn new() -> Self {
        let client = Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .user_agent("ironclad-jarvis/0.1")
            .build()
            .expect("reqwest client build cannot fail in default config");
        Self { client }
    }
}

#[async_trait]
impl Tool for WeatherTool {
    fn name(&self) -> &str {
        "weather"
    }

    fn description(&self) -> &str {
        "Get current conditions and forecast for any location via \
         Open-Meteo. Defaults to Tucson, AZ if `location` is omitted. \
         `mode` picks the granularity: 'current' for right-now, \
         'hourly' for the next N hours, 'daily' for the next N days, \
         'all' for everything (default 'all'). `days` and `hours` cap \
         the forecast horizon (max 7 days, 48 hours). `units` toggles \
         imperial (default, F + mph + in) vs metric (C + km/h + mm). \
         Read-only, no approval, no API key."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "location": {
                    "type": "string",
                    "description": "City name (e.g. 'Tucson', 'Phoenix, AZ', 'London') OR explicit 'lat,lon' (e.g. '32.22,-110.97'). Omit for Tucson, AZ."
                },
                "mode": {
                    "type": "string",
                    "enum": ["current", "hourly", "daily", "all"],
                    "description": "What to return. 'all' includes current + 3 daily + 12 hourly. Default 'all'."
                },
                "days": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 7,
                    "description": "Daily forecast horizon (1-7). Default 3."
                },
                "hours": {
                    "type": "integer",
                    "minimum": 1,
                    "maximum": 48,
                    "description": "Hourly forecast horizon (1-48). Default 12."
                },
                "units": {
                    "type": "string",
                    "enum": ["imperial", "metric"],
                    "description": "Default 'imperial'."
                }
            }
        })
    }

    async fn execute(
        &self,
        params: serde_json::Value,
        _ctx: &JobContext,
    ) -> Result<ToolOutput, ToolError> {
        let start = Instant::now();

        let mode = params
            .get("mode")
            .and_then(|v| v.as_str())
            .unwrap_or("all")
            .to_string();
        let days = params
            .get("days")
            .and_then(|v| v.as_u64())
            .unwrap_or(3)
            .clamp(1, 7) as u32;
        let hours = params
            .get("hours")
            .and_then(|v| v.as_u64())
            .unwrap_or(12)
            .clamp(1, 48) as u32;
        let units = params
            .get("units")
            .and_then(|v| v.as_str())
            .unwrap_or("imperial")
            .to_string();

        // Resolve location → (lat, lon, name).
        let location_param = params
            .get("location")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let (lat, lon, location_name) = match location_param {
            None => (DEFAULT_LAT, DEFAULT_LON, DEFAULT_LOCATION_NAME.to_string()),
            Some(s) => self.resolve_location(&s).await?,
        };

        // Pick units for the API call.
        let (temp_unit, wind_unit, precip_unit) = match units.as_str() {
            "metric" => ("celsius", "kmh", "mm"),
            _ => ("fahrenheit", "mph", "inch"),
        };

        // Build the Open-Meteo URL based on mode.
        let mut url = format!(
            "{}?latitude={}&longitude={}&temperature_unit={}&wind_speed_unit={}&precipitation_unit={}&timezone=auto",
            FORECAST_BASE, lat, lon, temp_unit, wind_unit, precip_unit
        );
        if mode == "current" || mode == "all" {
            url.push_str(
                "&current=temperature_2m,relative_humidity_2m,apparent_temperature,is_day,precipitation,weather_code,cloud_cover,wind_speed_10m,wind_direction_10m",
            );
        }
        if mode == "daily" || mode == "all" {
            url.push_str(&format!(
                "&daily=weather_code,temperature_2m_max,temperature_2m_min,sunrise,sunset,precipitation_sum,precipitation_probability_max,wind_speed_10m_max&forecast_days={}",
                days
            ));
        }
        if mode == "hourly" || mode == "all" {
            url.push_str(&format!(
                "&hourly=temperature_2m,precipitation_probability,precipitation,weather_code,wind_speed_10m&forecast_hours={}",
                hours
            ));
        }

        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::ExternalService(format!("Open-Meteo request: {e}")))?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            return Err(ToolError::ExternalService(format!(
                "Open-Meteo {} - {}",
                status, body
            )));
        }
        let raw: ForecastResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::ExternalService(format!("Open-Meteo decode: {e}")))?;

        // Build a voice-shaped JSON result.
        let mut out = serde_json::Map::new();
        out.insert(
            "location".to_string(),
            serde_json::Value::String(location_name),
        );
        out.insert(
            "coordinates".to_string(),
            serde_json::json!({ "lat": lat, "lon": lon }),
        );
        out.insert(
            "units".to_string(),
            serde_json::Value::String(units.clone()),
        );

        if let Some(cur) = raw.current {
            let condition = wmo_description(cur.weather_code, cur.is_day != 0);
            let wind_dir = bearing_to_compass(cur.wind_direction_10m);
            out.insert(
                "current".to_string(),
                serde_json::json!({
                    "temperature": cur.temperature_2m,
                    "feels_like": cur.apparent_temperature,
                    "condition": condition,
                    "weather_code": cur.weather_code,
                    "humidity_pct": cur.relative_humidity_2m,
                    "cloud_cover_pct": cur.cloud_cover,
                    "wind_speed": cur.wind_speed_10m,
                    "wind_direction": wind_dir,
                    "precipitation": cur.precipitation,
                    "is_day": cur.is_day != 0,
                    "as_of": cur.time,
                }),
            );
        }

        if let Some(daily) = raw.daily {
            let mut days_out: Vec<serde_json::Value> = Vec::new();
            for i in 0..daily.time.len() {
                let code = daily.weather_code.get(i).copied().unwrap_or(0);
                days_out.push(serde_json::json!({
                    "date": daily.time.get(i),
                    "high": daily.temperature_2m_max.get(i),
                    "low": daily.temperature_2m_min.get(i),
                    "condition": wmo_description(code, true),
                    "weather_code": code,
                    "sunrise": daily.sunrise.get(i),
                    "sunset": daily.sunset.get(i),
                    "precipitation_total": daily.precipitation_sum.get(i),
                    "rain_chance_pct": daily.precipitation_probability_max.get(i),
                    "wind_max": daily.wind_speed_10m_max.get(i),
                }));
            }
            out.insert("daily".to_string(), serde_json::Value::Array(days_out));
        }

        if let Some(hourly) = raw.hourly {
            let mut hours_out: Vec<serde_json::Value> = Vec::new();
            for i in 0..hourly.time.len() {
                let code = hourly.weather_code.get(i).copied().unwrap_or(0);
                hours_out.push(serde_json::json!({
                    "time": hourly.time.get(i),
                    "temperature": hourly.temperature_2m.get(i),
                    "rain_chance_pct": hourly.precipitation_probability.get(i),
                    "precipitation": hourly.precipitation.get(i),
                    "condition": wmo_description(code, true),
                    "weather_code": code,
                    "wind_speed": hourly.wind_speed_10m.get(i),
                }));
            }
            out.insert("hourly".to_string(), serde_json::Value::Array(hours_out));
        }

        Ok(ToolOutput::success(
            serde_json::Value::Object(out),
            start.elapsed(),
        ))
    }

    fn requires_sanitization(&self) -> bool {
        // Weather data is structured, factual, and non-user-controlled.
        // No need to run it through the injection-pattern sanitizer.
        false
    }
}

impl WeatherTool {
    /// Resolve a `location` parameter into (lat, lon, display name).
    /// Accepts either a city name (geocoded via Open-Meteo) or an
    /// explicit "lat,lon" string.
    async fn resolve_location(
        &self,
        raw: &str,
    ) -> Result<(f64, f64, String), ToolError> {
        let trimmed = raw.trim();

        // First try the "lat,lon" shortcut.
        if let Some((lat_str, lon_str)) = trimmed.split_once(',') {
            if let (Ok(lat), Ok(lon)) =
                (lat_str.trim().parse::<f64>(), lon_str.trim().parse::<f64>())
            {
                if (-90.0..=90.0).contains(&lat) && (-180.0..=180.0).contains(&lon) {
                    return Ok((lat, lon, format!("{:.4},{:.4}", lat, lon)));
                }
            }
        }

        // Otherwise geocode.
        let url = format!(
            "{}?name={}&count=1&language=en&format=json",
            GEOCODE_BASE,
            urlencoding::encode(trimmed)
        );
        let resp = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| ToolError::ExternalService(format!("geocode request: {e}")))?;
        if !resp.status().is_success() {
            return Err(ToolError::ExternalService(format!(
                "geocode returned {}",
                resp.status()
            )));
        }
        let body: GeocodeResponse = resp
            .json()
            .await
            .map_err(|e| ToolError::ExternalService(format!("geocode decode: {e}")))?;
        let first = body
            .results
            .and_then(|mut v| v.drain(..).next())
            .ok_or_else(|| {
                ToolError::ExecutionFailed(format!(
                    "no geocode match for '{}' — try 'City, Region' or explicit 'lat,lon'",
                    trimmed
                ))
            })?;
        let label = match (first.admin1.as_deref(), first.country.as_deref()) {
            (Some(state), _) => format!("{}, {}", first.name, state),
            (None, Some(country)) => format!("{}, {}", first.name, country),
            _ => first.name.clone(),
        };
        Ok((first.latitude, first.longitude, label))
    }
}

// ---------------- Open-Meteo response shapes ----------------

#[derive(Debug, Deserialize)]
struct ForecastResponse {
    #[serde(default)]
    current: Option<CurrentBlock>,
    #[serde(default)]
    daily: Option<DailyBlock>,
    #[serde(default)]
    hourly: Option<HourlyBlock>,
}

#[derive(Debug, Deserialize)]
struct CurrentBlock {
    time: String,
    temperature_2m: f64,
    apparent_temperature: f64,
    relative_humidity_2m: i32,
    cloud_cover: i32,
    is_day: i32,
    precipitation: f64,
    weather_code: i32,
    wind_speed_10m: f64,
    wind_direction_10m: f64,
}

#[derive(Debug, Deserialize)]
struct DailyBlock {
    time: Vec<String>,
    weather_code: Vec<i32>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
    sunrise: Vec<String>,
    sunset: Vec<String>,
    precipitation_sum: Vec<f64>,
    precipitation_probability_max: Vec<i32>,
    wind_speed_10m_max: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct HourlyBlock {
    time: Vec<String>,
    temperature_2m: Vec<f64>,
    precipitation_probability: Vec<i32>,
    precipitation: Vec<f64>,
    weather_code: Vec<i32>,
    wind_speed_10m: Vec<f64>,
}

#[derive(Debug, Deserialize)]
struct GeocodeResponse {
    results: Option<Vec<GeocodeResult>>,
}

#[derive(Debug, Deserialize)]
struct GeocodeResult {
    name: String,
    latitude: f64,
    longitude: f64,
    country: Option<String>,
    admin1: Option<String>,
}

// ---------------- helpers ----------------

/// Translate WMO weather code (Open-Meteo follows the same scheme) into
/// a short human-readable phrase suitable for voice output. `is_day`
/// branches a few codes (clear vs clear-night, partly-cloudy etc).
fn wmo_description(code: i32, is_day: bool) -> &'static str {
    match code {
        0 => {
            if is_day {
                "Clear sky"
            } else {
                "Clear night"
            }
        }
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
        61 => "Light rain",
        63 => "Moderate rain",
        65 => "Heavy rain",
        66 => "Light freezing rain",
        67 => "Heavy freezing rain",
        71 => "Light snow",
        73 => "Moderate snow",
        75 => "Heavy snow",
        77 => "Snow grains",
        80 => "Light rain showers",
        81 => "Moderate rain showers",
        82 => "Violent rain showers",
        85 => "Light snow showers",
        86 => "Heavy snow showers",
        95 => "Thunderstorm",
        96 => "Thunderstorm with light hail",
        99 => "Thunderstorm with heavy hail",
        _ => "Unknown conditions",
    }
}

/// Compass bearing → 16-point cardinal direction. Voice-friendly
/// ("northwest" is more useful than "315 degrees").
fn bearing_to_compass(deg: f64) -> &'static str {
    let normalized = ((deg % 360.0) + 360.0) % 360.0;
    let idx = ((normalized + 11.25) / 22.5).floor() as usize % 16;
    [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ][idx]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wmo_codes_have_known_meanings() {
        assert_eq!(wmo_description(0, true), "Clear sky");
        assert_eq!(wmo_description(0, false), "Clear night");
        assert_eq!(wmo_description(95, true), "Thunderstorm");
        assert_eq!(wmo_description(999, true), "Unknown conditions");
    }

    #[test]
    fn compass_bearings_round_correctly() {
        assert_eq!(bearing_to_compass(0.0), "N");
        assert_eq!(bearing_to_compass(45.0), "NE");
        assert_eq!(bearing_to_compass(180.0), "S");
        assert_eq!(bearing_to_compass(225.0), "SW");
        assert_eq!(bearing_to_compass(360.0), "N");
        assert_eq!(bearing_to_compass(-45.0), "NW");
    }
}
