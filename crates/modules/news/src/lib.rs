//! News module: headlines from one or more RSS/Atom feeds (no API key required).
//!
//! The view shows one headline at a time and rotates every 30 seconds,
//! cycling back to the first headline after the last. Fetching happens on a
//! slower cadence: a feed is only re-fetched when the last fetch is stale.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use futures::future::join_all;
use quick_xml::events::Event;
use quick_xml::Reader;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use smmm_core::{Module, ModuleCtx};
use smmm_protocol::{
    Align, Manifest, ModuleStatus, Region, TextSize, ToolDescriptor, ViewNode, Weight,
};
use url::Url;

/// How often the view advances to the next headline.
const UPDATE_INTERVAL: Duration = Duration::from_secs(30);
/// Re-fetch feeds only when the last fetch is at least this old.
const REFRESH_AFTER: Duration = Duration::from_secs(10 * 60);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(10);

/// One configured feed source.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeedConfig {
    /// RSS 2.0 or Atom feed URL.
    pub url: String,
    /// Source name shown with each headline (e.g. `"BBC"`). When omitted the
    /// feed's host name is used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NewsConfig {
    /// Caption shown above the headlines.
    pub label: String,
    /// Feed sources to pull headlines from, merged newest-first.
    pub feeds: Vec<FeedConfig>,
    /// Headlines considered for rotation, clamped to 1..=30.
    pub max_items: usize,
    /// Show a relative age ("2h ago") with each headline when the feed
    /// provides a usable timestamp.
    pub show_age: bool,
}

impl Default for NewsConfig {
    fn default() -> Self {
        Self {
            label: "News".to_string(),
            feeds: vec![
                FeedConfig {
                    url: "https://feeds.bbci.co.uk/news/rss.xml".to_string(),
                    label: Some("BBC".to_string()),
                },
                FeedConfig {
                    url: "https://www.theverge.com/rss/index.xml".to_string(),
                    label: Some("The Verge".to_string()),
                },
            ],
            max_items: 30,
            show_age: true,
        }
    }
}

impl NewsConfig {
    /// The feeds to poll, with a display label resolved for each.
    fn resolved_feeds(&self) -> Vec<(String, String)> {
        self.feeds
            .iter()
            .filter(|feed| !feed.url.is_empty())
            .map(|feed| {
                let label = feed
                    .label
                    .clone()
                    .filter(|label| !label.is_empty())
                    .unwrap_or_else(|| host_of(&feed.url));
                (feed.url.clone(), label)
            })
            .collect()
    }
}

/// `"https://feeds.example.com/x.xml"` → `"example.com"`.
fn host_of(raw: &str) -> String {
    Url::parse(raw)
        .ok()
        .and_then(|parsed| parsed.host_str().map(str::to_string))
        .unwrap_or_else(|| raw.to_string())
        .trim_start_matches("www.")
        .to_string()
}

/// One parsed headline from one feed.
struct Headline {
    title: String,
    source: String,
    published: Option<DateTime<Utc>>,
}

pub struct NewsModule {
    client: reqwest::Client,
    ctx: Option<ModuleCtx>,
    config: NewsConfig,
    visible: bool,
    view: ViewNode,
    status: ModuleStatus,
    last_updated: Option<String>,
    /// When the last successful fetch happened.
    last_fetch: Option<Instant>,
    /// Merged headlines across feeds, newest first.
    headlines: Vec<Headline>,
    /// Which headline the view is currently showing.
    cursor: usize,
}

impl NewsModule {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::new(),
            ctx: None,
            config: NewsConfig::default(),
            visible: true,
            view: ViewNode::Empty,
            status: ModuleStatus::Starting,
            last_updated: None,
            last_fetch: None,
            headlines: Vec::new(),
            cursor: 0,
        }
    }

    fn max_items(&self) -> usize {
        self.config.max_items.clamp(1, 30)
    }

    /// The headline the view is currently showing.
    fn current(&self) -> Option<&Headline> {
        self.headlines.get(self.cursor % self.headlines.len().max(1))
    }

    /// What the view is showing right now, for the `next`/`previous` tools.
    fn display_json(&self) -> Value {
        match self.current() {
            Some(headline) => json!({
                "title": headline.title,
                "source": headline.source,
                "age": headline.published.map(age_string),
                "position": format!(
                    "{}/{}",
                    self.cursor % self.headlines.len() + 1,
                    self.headlines.len()
                ),
            }),
            None => json!({ "on_display": null }),
        }
    }

    /// Advance the rotation by `delta` steps, wrapping in both directions.
    fn rotate(&mut self, delta: isize) {
        if self.headlines.is_empty() {
            return;
        }
        let len = self.headlines.len() as isize;
        self.cursor = (self.cursor as isize + delta).rem_euclid(len) as usize;
        self.view = self.render();
    }

    /// Fetch every configured feed concurrently, merge the results, and rebuild
    /// the view. Never fails: errors become a degraded status so the module
    /// (and the rest of the mirror) keeps running.
    async fn refresh(&mut self) {
        let feeds = self.config.resolved_feeds();
        let fetches = feeds
            .iter()
            .map(|(url, source)| fetch_feed(&self.client, url, source.clone()));
        let results = join_all(fetches).await;

        let mut headlines: Vec<Headline> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for (index, result) in results.into_iter().enumerate() {
            let source = feeds
                .get(index)
                .map(|(_, label)| label.clone())
                .unwrap_or_default();
            match result {
                Ok(items) => {
                    if let Some(ctx) = &self.ctx {
                        ctx.info(format!("fetched {source}: {} headlines", items.len()));
                    }
                    headlines.extend(items);
                }
                Err(e) => failures.push(e.to_string()),
            }
        }
        headlines = merge(headlines, self.max_items());

        if let Some(ctx) = &self.ctx {
            for failure in &failures {
                ctx.warn(format!("feed failed: {failure:#}"));
            }
        }

        if headlines.is_empty() {
            // Every feed failed. Keep showing whatever was on screen before.
            let message = failures.first().map(String::as_str).unwrap_or("no feeds configured");
            if let Some(ctx) = &self.ctx {
                ctx.warn(format!("fetch failed: {message}"));
            }
            self.status = ModuleStatus::Degraded {
                message: message.to_string(),
            };
            if matches!(self.view, ViewNode::Empty) {
                self.view = error_view(message);
            }
            return;
        }

        self.headlines = headlines;
        self.cursor = 0;
        self.view = self.render();
        self.status = ModuleStatus::Ok;
        self.last_updated = Some(chrono::Local::now().format("%H:%M").to_string());
        self.last_fetch = Some(Instant::now());
    }

    /// Headlines for `get_headlines`.
    fn headlines_json(&self) -> Option<Value> {
        if self.headlines.is_empty() {
            return None;
        }
        let items: Vec<Value> = self
            .headlines
            .iter()
            .enumerate()
            .map(|(index, headline)| {
                json!({
                    "title": headline.title,
                    "source": headline.source,
                    "age": headline.published.map(age_string),
                    "published": headline.published.map(|t| t.to_rfc3339()),
                    "on_display": index == self.cursor % self.headlines.len(),
                })
            })
            .collect();
        Some(json!({
            "source": self.config.label,
            "items": items,
            "updated": self.last_updated,
        }))
    }

    fn render(&self) -> ViewNode {
        let Some(headline) = self.current() else {
            return ViewNode::Empty;
        };

        let mut children = Vec::new();

        if !self.config.label.is_empty() {
            children.push(ViewNode::styled(
                self.config.label.as_str(),
                TextSize::Sm,
                Weight::Light,
                Align::Center,
            ));
        }

        children.push(ViewNode::styled(
            headline.title.as_str(),
            TextSize::Lg,
            Weight::Light,
            Align::Center,
        ));

        // Source and age share one muted line: "BBC · 2h ago".
        let mut detail = vec![headline.source.clone()];
        if self.config.show_age {
            if let Some(published) = headline.published {
                detail.push(age_string(published));
            }
        }
        children.push(ViewNode::styled(
            detail.join(" · "),
            TextSize::Sm,
            Weight::Light,
            Align::Center,
        ));

        ViewNode::column(4.0, children)
    }
}

/// Fetch one feed and tag its headlines with a source label.
async fn fetch_feed(
    client: &reqwest::Client,
    url: &str,
    source: String,
) -> anyhow::Result<Vec<Headline>> {
    let response = client.get(url).send().await?.error_for_status()?;
    let xml = response.text().await?;
    Ok(parse_feed(&xml)
        .into_iter()
        .map(|headline| Headline {
            source: source.clone(),
            ..headline
        })
        .collect())
}

/// Deduplicate by title, sort newest-first, and cap the count.
fn merge(headlines: Vec<Headline>, max_items: usize) -> Vec<Headline> {
    let mut seen = HashSet::new();
    let mut merged: Vec<Headline> = headlines
        .into_iter()
        .filter(|headline| seen.insert(headline.title.to_lowercase()))
        .collect();
    // Newest first; headlineless entries sink to the end.
    merged.sort_by(|a, b| b.published.cmp(&a.published));
    let cap = max_items.clamp(1, 30);
    merged.truncate(cap);
    merged
}

impl Default for NewsModule {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Module for NewsModule {
    fn manifest(&self) -> Manifest {
        Manifest {
            id: "news".to_string(),
            name: "News".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            description: "Rotating headlines from one or more RSS/Atom feeds.".to_string(),
            icon: Some("news".to_string()),
            default_region: Region::LowerThird,
            default_order: 20,
            default_visible: true,
            config_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "label": { "type": "string", "title": "Label", "default": "News" },
                    "feeds": {
                        "type": "array",
                        "title": "Feeds",
                        "description": "Feed sources, merged newest-first.",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "url": { "type": "string", "title": "Feed URL" },
                                "label": { "type": "string", "title": "Source label" }
                            },
                            "required": ["url"]
                        }
                    },
                    "max_items": {
                        "type": "integer",
                        "title": "Headlines",
                        "default": 30,
                        "minimum": 1,
                        "maximum": 30,
                        "description": "Total headlines considered for rotation."
                    },
                    "show_age": {
                        "type": "boolean",
                        "title": "Show age",
                        "default": true,
                        "description": "Relative age (\"2h ago\") with each headline."
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
                "next",
                "Next headline",
                "Show the next headline now, instead of waiting for the 30-second rotation.",
            ),
            ToolDescriptor::action(
                "previous",
                "Previous headline",
                "Step back to the headline before the one on display.",
            ),
            ToolDescriptor::action(
                "refresh",
                "Refresh now",
                "Fetch every configured feed immediately instead of waiting for the next poll.",
            ),
            ToolDescriptor::query(
                "get_headlines",
                "Get headlines",
                "Read the merged headlines from the last fetch. Does not hit the network; call \
                 `refresh` first if it looks stale.",
            )
            .returns(json!({
                "type": "object",
                "properties": { "items": { "type": "array", "items": { "type": "object" } } }
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

    /// Runs every 30 seconds: advance the rotation, and fetch fresh headlines
    /// only when the last fetch is stale.
    async fn update(&mut self) -> anyhow::Result<()> {
        let stale = self
            .last_fetch
            .map_or(true, |fetched| fetched.elapsed() >= REFRESH_AFTER);
        if stale || self.headlines.is_empty() {
            self.refresh().await;
        } else {
            self.rotate(1);
        }
        Ok(())
    }

    async fn on_config(&mut self, config: Value) -> anyhow::Result<()> {
        self.config = if config.is_null() {
            NewsConfig::default()
        } else {
            serde_json::from_value(config)?
        };
        self.refresh().await;
        Ok(())
    }

    async fn call_tool(&mut self, tool: &str, _args: Value) -> anyhow::Result<Value> {
        match tool {
            "next" => {
                self.rotate(1);
                Ok(self.display_json())
            }
            "previous" => {
                self.rotate(-1);
                Ok(self.display_json())
            }
            "refresh" => {
                self.refresh().await;
                Ok(json!({
                    "status": self.status,
                    "updated": self.last_updated,
                }))
            }
            "get_headlines" => self
                .headlines_json()
                .ok_or_else(|| anyhow::anyhow!("no headlines yet — call `refresh` first")),
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
            ViewNode::styled("News", TextSize::Sm, Weight::Light, Align::Center),
            ViewNode::styled("Unavailable", TextSize::Lg, Weight::Normal, Align::Center),
            ViewNode::key_value([("Error", message.to_string())]),
        ],
    )
}

// ---------------------------------------------------------------------------
// Feed parsing
// ---------------------------------------------------------------------------

/// Parse the `<item>` (RSS 2.0) or `<entry>` (Atom) titles from a feed, in
/// feed order. Element and attribute names are matched by local name so
/// namespaced Atom documents parse the same as plain ones. Malformed XML
/// yields whatever was parsed before the error rather than failing the update.
fn parse_feed(xml: &str) -> Vec<Headline> {
    let mut reader = Reader::from_str(xml);
    let mut headlines = Vec::new();

    let mut in_entry = false;
    let mut title = String::new();
    let mut date = String::new();
    // Which of the two strings the current text belongs to, if any.
    let mut capture: Option<Capture> = None;

    loop {
        match reader.read_event() {
            Ok(Event::Start(ref e)) => match e.local_name().as_ref() {
                b"item" | b"entry" => {
                    in_entry = true;
                    title.clear();
                    date.clear();
                }
                b"title" if in_entry => capture = Some(Capture::Title),
                // A feed may carry both `updated` and `published` per entry;
                // keep the first one seen instead of concatenating them.
                b"pubDate" | b"updated" | b"published" | b"date" if in_entry && date.is_empty() => {
                    capture = Some(Capture::Date)
                }
                _ => {}
            },
            Ok(Event::Text(ref e)) => {
                let text = e.unescape().map(|t| t.into_owned()).unwrap_or_default();
                append_captured(&capture, &mut title, &mut date, &text);
            }
            // CData is already raw; everything else may hold entities.
            Ok(Event::CData(ref e)) => {
                let text = String::from_utf8_lossy(&e[..]).into_owned();
                append_captured(&capture, &mut title, &mut date, &text);
            }
            Ok(Event::End(ref e)) => match e.local_name().as_ref() {
                b"title" | b"pubDate" | b"updated" | b"published" | b"date" => capture = None,
                b"item" | b"entry" => {
                    in_entry = false;
                    let title = title.trim().to_string();
                    if !title.is_empty() {
                        let published = parse_timestamp(date.trim());
                        headlines.push(Headline {
                            title,
                            source: String::new(),
                            published,
                        });
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    headlines
}

enum Capture {
    Title,
    Date,
}

fn append_captured(capture: &Option<Capture>, title: &mut String, date: &mut String, text: &str) {
    match capture {
        Some(Capture::Title) => title.push_str(text),
        Some(Capture::Date) => date.push_str(text),
        None => {}
    }
}

/// Accepts RSS `pubDate` (RFC 2822: `Tue, 12 Nov 2026 09:30:00 GMT`) and Atom
/// `updated`/`published` (RFC 3339).
///
/// chrono's strict RFC 2822 parser rejects the 4-digit-year form nearly every
/// feed emits, so common `%a, %d %b %Y` variants are tried as fallbacks.
fn parse_timestamp(raw: &str) -> Option<DateTime<Utc>> {
    const FALLBACKS: &[&str] = &["%a, %d %b %Y %H:%M:%S %z", "%a, %d %b %Y %H:%M %z"];

    let parsed = DateTime::parse_from_rfc2822(raw)
        .ok()
        .or_else(|| {
            FALLBACKS
                .iter()
                .find_map(|format| DateTime::parse_from_str(raw, format).ok())
        })
        .or_else(|| DateTime::parse_from_rfc3339(raw).ok());
    parsed.map(|time| time.with_timezone(&Utc))
}

/// Relative age for a timestamp: `just now`, `12m ago`, `3h ago`, `2d ago`.
fn age_string(published: DateTime<Utc>) -> String {
    let minutes = Utc::now().signed_duration_since(published).num_minutes();
    if minutes < 1 {
        "just now".to_string()
    } else if minutes < 60 {
        format!("{minutes}m ago")
    } else if minutes < 24 * 60 {
        format!("{}h ago", minutes / 60)
    } else {
        format!("{}d ago", minutes / (24 * 60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_rss_items() {
        // 12 Nov 2026 is a Thursday — chrono validates the weekday against the
        // date, so the fixture must agree.
        let xml = r#"
            <rss><channel>
                <title>Example</title>
                <item><title>First &amp; foremost</title><pubDate>Thu, 12 Nov 2026 09:30:00 GMT</pubDate></item>
                <item><title><![CDATA[Second story]]></title></item>
            </channel></rss>
        "#;
        let headlines = parse_feed(xml);
        assert_eq!(headlines.len(), 2);
        assert_eq!(headlines[0].title, "First & foremost");
        assert!(headlines[0].published.is_some());
        assert_eq!(headlines[1].title, "Second story");
        assert!(headlines[1].published.is_none());
    }

    #[test]
    fn parses_atom_entries() {
        let xml = r#"
            <feed xmlns="http://www.w3.org/2005/Atom">
                <entry><title>Hello</title><updated>2026-11-12T09:30:00Z</updated></entry>
                <entry><title>World</title></entry>
            </feed>
        "#;
        let headlines = parse_feed(xml);
        assert_eq!(headlines.len(), 2);
        assert!(headlines[0].published.is_some());
        assert!(headlines[1].published.is_none());
    }

    #[test]
    fn ignores_channel_title() {
        let xml = "<rss><channel><title>Channel name</title></channel></rss>";
        assert!(parse_feed(xml).is_empty());
    }

    #[test]
    fn rotation_wraps_in_both_directions() {
        let mut module = NewsModule::new();
        module.headlines = vec![
            headline("First", "S", None),
            headline("Second", "S", None),
            headline("Third", "S", None),
        ];

        module.rotate(1);
        assert_eq!(module.current().unwrap().title, "Second");

        module.rotate(-1);
        assert_eq!(module.current().unwrap().title, "First");

        // Back past the start wraps to the end.
        module.rotate(-1);
        assert_eq!(module.current().unwrap().title, "Third");

        // Forward from there wraps to the start.
        module.rotate(1);
        assert_eq!(module.current().unwrap().title, "First");
    }

    #[test]
    fn atom_entry_with_updated_and_published_keeps_one_timestamp() {
        // The Verge-style entry carries both elements; concatenating them
        // produced an unparseable timestamp (regression).
        let xml = r#"
            <feed xmlns="http://www.w3.org/2005/Atom">
                <entry>
                    <title>Both dates</title>
                    <updated>2026-11-15T20:47:16-05:00</updated>
                    <published>2026-11-15T20:26:56-05:00</published>
                </entry>
            </feed>
        "#;
        let headlines = parse_feed(xml);
        assert_eq!(headlines.len(), 1);
        assert!(headlines[0].published.is_some());
    }

    #[test]
    fn parses_common_timestamp_shapes() {
        // 4-digit year with weekday — the shape most feeds emit, which chrono's
        // strict RFC 2822 parser rejects.
        assert!(parse_timestamp("Thu, 12 Nov 2026 09:30:00 GMT").is_some());
        assert!(parse_timestamp("Thu, 12 Nov 2026 09:30 +0000").is_some());
        // Strict RFC 2822 (2-digit year).
        assert!(parse_timestamp("12 Nov 26 09:30:00 GMT").is_some());
        // Atom.
        assert!(parse_timestamp("2026-11-12T09:30:00Z").is_some());
        // Not a timestamp at all.
        assert!(parse_timestamp("yesterday").is_none());
    }

    fn headline(title: &str, source: &str, minutes_ago: Option<i64>) -> Headline {
        Headline {
            title: title.to_string(),
            source: source.to_string(),
            published: minutes_ago.map(|mins| Utc::now() - chrono::Duration::minutes(mins)),
        }
    }

    #[test]
    fn merges_sources_newest_first_without_duplicates() {
        let merged = merge(
            vec![
                headline("Old story", "BBC", Some(300)),
                headline("Fresh story", "The Verge", Some(10)),
                headline("Old story", "BBC", Some(299)), // duplicate title
                headline("Undated story", "HN", None),
            ],
            10,
        );
        let titles: Vec<&str> = merged.iter().map(|h| h.title.as_str()).collect();
        assert_eq!(titles, ["Fresh story", "Old story", "Undated story"]);
        // The surviving copy of the duplicate keeps its source.
        assert_eq!(merged[1].source, "BBC");
    }

    #[test]
    fn merge_caps_count() {
        let merged = merge(
            (0..15).map(|i| headline(&format!("Story {i}"), "BBC", Some(i as i64))).collect(),
            5,
        );
        assert_eq!(merged.len(), 5);
        assert_eq!(merged[0].title, "Story 0"); // newest
    }
}
