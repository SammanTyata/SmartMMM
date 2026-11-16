//! Weather module: current conditions from Open-Meteo (no API key required).

use std::time::Duration;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smmm_core::{Module, ModuleCtx};
use smmm_protocol::{
    Align, Axis, Manifest, ModuleStatus, Region, TextSize, ToolDescriptor, ViewNode, Weight,
};

const UPDATE_INTERVAL: Duration = Duration::from_secs(15 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Units {
    #[default]
    Metric,
    Imperial,
}

impl Units {
    fn as_str(self) -> &'static str {
        match self {
            Units::Metric => "metric",
            Units::Imperial => "imperial",
        }
    }

    fn temperature_param(self) -> &'static str {
        match self {
            Units::Metric => "celsius",
            Units::Imperial => "fahrenheit",
        }
    }

    fn wind_param(self) -> &'static str {
        match self {
            Units::Metric => "kmh",
            Units::Imperial => "mph",
        }
    }

    fn temperature_suffix(self) -> &'static str {
        match self {
            Units::Metric => "°C",
            Units::Imperial => "°F",
        }
    }

    fn wind_suffix(self) -> &'static str {
        match self {
            Units::Metric => "km/h",
            Units::Imperial => "mph",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct WeatherConfig {
    /// Place name shown above the temperature.
    ///
    /// Purely cosmetic. The API is queried by coordinate, never by name, so this
    /// is not verified against `latitude`/`longitude`.
    pub label: String,
    pub latitude: f64,
    pub longitude: f64,
    pub units: Units,
    /// Days of daily forecast to fetch, clamped to 1..=7 by the API.
    pub forecast_days: u8,
}

impl Default for WeatherConfig {
    fn default() -> Self {
        Self {
            label: "Amsterdam".to_string(),
            latitude: 52.37,
            longitude: 4.90,
            units: Units::Metric,
            forecast_days: 3,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OpenMeteoResponse {
    current: CurrentWeather,
    #[serde(default)]
    daily: Option<DailyForecast>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CurrentWeather {
    temperature_2m: f64,
    relative_humidity_2m: f64,
    apparent_temperature: f64,
    weather_code: i32,
    wind_speed_10m: f64,
}

/// Open-Meteo returns the daily block as parallel arrays, one entry per day.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DailyForecast {
    /// ISO dates, e.g. `["2026-10-15", "2026-10-16"]`.
    time: Vec<String>,
    weather_code: Vec<i32>,
    temperature_2m_max: Vec<f64>,
    temperature_2m_min: Vec<f64>,
}

/// One successful fetch: current conditions plus the daily forecast.
struct Observation {
    current: CurrentWeather,
    daily: Option<DailyForecast>,
}

pub struct WeatherModule {
    client: reqwest::Client,
    ctx: Option<ModuleCtx>,
    config: WeatherConfig,
    visible: bool,
    view: ViewNode,
    status: ModuleStatus,
    last_updated: Option<String>,
    /// Last successful observation, exposed through `get_conditions`.
    last_conditions: Option<CurrentWeather>,
    /// Last successful daily forecast, exposed through `get_forecast`.
    last_forecast: Option<DailyForecast>,
}

impl WeatherModule {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            ctx: None,
            config: WeatherConfig::default(),
            visible: true,
            view: ViewNode::Empty,
            status: ModuleStatus::Starting,
            last_updated: None,
            last_conditions: None,
            last_forecast: None,
        }
    }

    /// Open-Meteo accepts 1..=16 days; we keep it tighter than that so the
    /// result still fits in a phone panel and in a model's context window.
    fn forecast_days(&self) -> u8 {
        self.config.forecast_days.clamp(1, 7)
    }

    fn endpoint(&self) -> String {
        format!(
            "https://api.open-meteo.com/v1/forecast?latitude={lat}&longitude={lon}\
             &current=temperature_2m,relative_humidity_2m,apparent_temperature,weather_code,wind_speed_10m\
             &daily=weather_code,temperature_2m_max,temperature_2m_min\
             &forecast_days={days}\
             &temperature_unit={t}&wind_speed_unit={w}&timezone=auto",
            lat = self.config.latitude,
            lon = self.config.longitude,
            days = self.forecast_days(),
            t = self.config.units.temperature_param(),
            w = self.config.units.wind_param(),
        )
    }

    /// Fetch and rebuild the view. Never fails: errors become a degraded status
    /// so the module (and the rest of the mirror) keeps running.
    async fn refresh(&mut self) {
        match self.fetch().await {
            Ok(observation) => {
                self.view = self.render(&observation);
                self.status = ModuleStatus::Ok;
                self.last_updated = Some(chrono::Local::now().format("%H:%M").to_string());
                self.last_conditions = Some(observation.current);
                self.last_forecast = observation.daily;
            }
            Err(e) => {
                if let Some(ctx) = &self.ctx {
                    ctx.warn(format!("fetch failed: {e:#}"));
                }
                self.status = ModuleStatus::Degraded {
                    message: e.to_string(),
                };
                if matches!(self.view, ViewNode::Empty) {
                    self.view = error_view(&e.to_string());
                }
            }
        }
    }

    async fn fetch(&self) -> anyhow::Result<Observation> {
        let response = self
            .client
            .get(self.endpoint())
            .send()
            .await?
            .error_for_status()?;
        let data: OpenMeteoResponse = response.json().await?;
        Ok(Observation {
            current: data.current,
            daily: data.daily,
        })
    }

    /// Current conditions, for `get_conditions`.
    fn conditions_json(&self) -> Option<Value> {
        let current = self.last_conditions.as_ref()?;
        Some(json!({
            "location": self.config.label,
            "units": self.config.units.as_str(),
            "temperature": current.temperature_2m,
            "feels_like": current.apparent_temperature,
            "humidity": current.relative_humidity_2m,
            "wind_speed": current.wind_speed_10m,
            "condition": weather_code_label(current.weather_code),
            "weather_code": current.weather_code,
            "updated": self.last_updated,
        }))
    }

    /// Multi-day daily forecast, for `get_forecast`.
    fn forecast_json(&self) -> Option<Value> {
        let daily = self.last_forecast.as_ref()?;
        let days: Vec<Value> = (0..daily.time.len())
            .map(|i| {
                let code = daily.weather_code.get(i).copied().unwrap_or(0);
                json!({
                    "date": daily.time.get(i),
                    "high": daily.temperature_2m_max.get(i),
                    "low": daily.temperature_2m_min.get(i),
                    "condition": weather_code_label(code),
                    "weather_code": code,
                })
            })
            .collect();
        Some(json!({
            "location": self.config.label,
            "units": self.config.units.as_str(),
            "days": days,
            "updated": self.last_updated,
        }))
    }

    fn render(&self, observation: &Observation) -> ViewNode {
        let current = &observation.current;
        let units = self.config.units;
        let label = if self.config.label.is_empty() {
            "Weather"
        } else {
            self.config.label.as_str()
        };

        let mut children = vec![
            ViewNode::styled(label, TextSize::Sm, Weight::Light, Align::Center),
            // Icon beside the temperature: the temperature stays the focal
            // point, the emoji gives it a glanceable glyph.
            ViewNode::stack(
                Axis::Horizontal,
                10.0,
                vec![
                    ViewNode::Icon {
                        name: weather_code_emoji(current.weather_code).to_string(),
                        size: Some(52.0),
                    },
                    ViewNode::styled(
                        format!(
                            "{:.0}{}",
                            current.temperature_2m,
                            units.temperature_suffix()
                        ),
                        TextSize::Xxl,
                        Weight::Light,
                        Align::Center,
                    ),
                ],
            ),
            ViewNode::styled(
                weather_code_label(current.weather_code),
                TextSize::Md,
                Weight::Normal,
                Align::Center,
            ),
            ViewNode::key_value([
                (
                    "Feels like",
                    format!(
                        "{:.0}{}",
                        current.apparent_temperature,
                        units.temperature_suffix()
                    ),
                ),
                ("Humidity", format!("{:.0}%", current.relative_humidity_2m)),
                (
                    "Wind",
                    format!("{:.0} {}", current.wind_speed_10m, units.wind_suffix()),
                ),
            ]),
        ];

        // The daily forecast is fetched on every update; show it when present.
        if let Some(daily) = &observation.daily {
            children.push(ViewNode::Divider);
            children.push(self.forecast_strip(daily));
        }

        ViewNode::column(4.0, children)
    }

    /// Horizontal row of day columns: name, emoji, high/low. Skips a day's
    /// temperatures only when the API omitted them.
    fn forecast_strip(&self, daily: &DailyForecast) -> ViewNode {
        let today = chrono::Local::now().date_naive();
        let days: Vec<ViewNode> = daily
            .time
            .iter()
            .enumerate()
            .map(|(i, date)| {
                let code = daily.weather_code.get(i).copied().unwrap_or(0);
                let temps = match (
                    daily.temperature_2m_max.get(i),
                    daily.temperature_2m_min.get(i),
                ) {
                    (Some(high), Some(low)) => format!("{high:.0}°/{low:.0}°"),
                    (Some(high), None) => format!("{high:.0}°"),
                    (None, _) => String::new(),
                };
                ViewNode::column(
                    2.0,
                    vec![
                        ViewNode::styled(
                            day_label(date, today),
                            TextSize::Sm,
                            Weight::Light,
                            Align::Center,
                        ),
                        ViewNode::Icon {
                            name: weather_code_emoji(code).to_string(),
                            size: None,
                        },
                        ViewNode::styled(temps, TextSize::Sm, Weight::Normal, Align::Center),
                    ],
                )
            })
            .collect();
        ViewNode::stack(Axis::Horizontal, 14.0, days)
    }
}

impl Default for WeatherModule {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Module for WeatherModule {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "weather".to_string(),
            name: "Weather".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Current conditions and daily forecast from Open-Meteo.".to_string(),
            icon: Some("cloud".to_string()),
            default_region: Region::TopRight,
            default_order: 10,
            default_visible: true,
            config_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string", "title": "Label", "default": "Amsterdam" },
                    "latitude": { "type": "number", "title": "Latitude", "default": 52.37 },
                    "longitude": { "type": "number", "title": "Longitude", "default": 4.90 },
                    "units": {
                        "type": "string",
                        "title": "Units",
                        "enum": ["metric", "imperial"],
                        "default": "metric"
                    },
                    "forecast_days": {
                        "type": "integer",
                        "title": "Forecast days",
                        "default": 3,
                        "minimum": 1,
                        "maximum": 7,
                        "description": "Days returned by weather__get_forecast."
                    }
                }
            }),
        }
    }

    fn update_interval(&self) -> Duration {
        UPDATE_INTERVAL
    }

    fn tools(&self) -> Vec<ToolDescriptor> {
        vec![
            ToolDescriptor::action(
                "refresh",
                "Refresh now",
                "Fetch the latest weather immediately instead of waiting for the next poll.",
            ),
            ToolDescriptor::query(
                "get_conditions",
                "Get conditions",
                "Read the current temperature, feels-like, humidity and wind at the configured \
                 location, as of the last fetch. Does not hit the network; call `refresh` first \
                 if it looks stale.",
            )
            .returns(json!({ "type": "object" })),
            ToolDescriptor::query(
                "get_forecast",
                "Get forecast",
                "Read the daily forecast (high, low and conditions per day) for the configured \
                 location, as of the last fetch. The number of days comes from the module's \
                 `forecast_days` config. Does not hit the network.",
            )
            .returns(json!({
                "type": "object",
                "properties": { "days": { "type": "array", "items": { "type": "object" } } }
            })),
        ]
    }

    async fn init(&mut self, ctx: ModuleCtx) -> anyhow::Result<()> {
        self.client = reqwest::Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .user_agent(concat!("smartmmm/", env!("CARGO_PKG_VERSION")))
            .build()?;
        self.ctx = Some(ctx);
        Ok(())
    }

    async fn update(&mut self) -> anyhow::Result<()> {
        self.refresh().await;
        Ok(())
    }

    async fn on_config(&mut self, config: Value) -> anyhow::Result<()> {
        self.config = if config.is_null() {
            WeatherConfig::default()
        } else {
            serde_json::from_value(config)?
        };
        self.refresh().await;
        Ok(())
    }

    async fn call_tool(&mut self, tool: &str, _args: Value) -> anyhow::Result<Value> {
        match tool {
            "refresh" => {
                self.refresh().await;
                Ok(json!({
                    "status": self.status,
                    "updated": self.last_updated,
                }))
            }
            "get_conditions" => self
                .conditions_json()
                .ok_or_else(|| anyhow::anyhow!("no weather data yet — call `refresh` first")),
            "get_forecast" => self
                .forecast_json()
                .ok_or_else(|| anyhow::anyhow!("no forecast data yet — call `refresh` first")),
            other => anyhow::bail!("unknown tool `{other}`"),
        }
    }

    fn config(&self) -> Option<Value> {
        // Report the running config so the store (and the phone's config form)
        // reflects defaults the module chose, not just what was sent.
        serde_json::to_value(&self.config).ok()
    }

    fn visible(&self) -> bool {
        self.visible
    }

    fn set_visible(&mut self, visible: bool) {
        self.visible = visible;
    }

    fn view(&self) -> ViewNode {
        self.view.clone()
    }

    fn status(&self) -> ModuleStatus {
        self.status.clone()
    }
}

fn error_view(message: &str) -> ViewNode {
    ViewNode::column(
        4.0,
        vec![
            ViewNode::styled("Weather", TextSize::Sm, Weight::Light, Align::Center),
            ViewNode::styled("Unavailable", TextSize::Lg, Weight::Normal, Align::Center),
            ViewNode::key_value([("Error", message.to_string())]),
        ],
    )
}

/// WMO weather interpretation codes used by Open-Meteo.
fn weather_code_label(code: i32) -> &'static str {
    match code {
        0 => "Clear sky",
        1 => "Mainly clear",
        2 => "Partly cloudy",
        3 => "Overcast",
        45 | 48 => "Fog",
        51 | 53 | 55 => "Drizzle",
        56 | 57 => "Freezing drizzle",
        61 | 63 | 65 => "Rain",
        66 | 67 => "Freezing rain",
        71 | 73 | 75 => "Snow",
        77 => "Snow grains",
        80..=82 => "Rain showers",
        85 | 86 => "Snow showers",
        95 => "Thunderstorm",
        96 | 99 => "Thunderstorm with hail",
        _ => "Unknown",
    }
}

/// Emoji glyph for each WMO code, used beside the temperature and in the
/// forecast strip. Emoji render as color glyphs, so they read on any theme.
fn weather_code_emoji(code: i32) -> &'static str {
    match code {
        0 => "☀️",
        1 => "🌤️",
        2 => "⛅",
        3 => "☁️",
        45 | 48 => "🌫️",
        51 | 53 | 55 => "🌦️",
        56 | 57 | 66 | 67 => "🌧️",
        71 | 73 | 75 | 77 => "❄️",
        80..=82 => "🌧️",
        85 | 86 => "🌨️",
        95 | 96 | 99 => "⛈️",
        _ => "🌡️",
    }
}

/// `"2026-11-16"` → `"Today"` on the matching date, otherwise the weekday
/// abbreviation (`"Mon"`). Falls back to the raw ISO date if parsing fails.
fn day_label(date: &str, today: chrono::NaiveDate) -> String {
    match chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d") {
        Ok(day) if day == today => "Today".to_string(),
        Ok(day) => day.format("%a").to_string(),
        Err(_) => date.to_string(),
    }
}
