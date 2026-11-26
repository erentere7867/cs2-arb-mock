//! Read-only sources of CS2 (Steam App ID 730) branch metadata.
//!
//! # Verified vs. unverified
//!
//! * [`SteamCmdApiSource`] — `GET https://api.steamcmd.net/v1/info/730`, a
//!   public JSON proxy of Steam PICS product info. The response shape was
//!   observed on 2026-11-26: `{"status":"success","data":{"730":{"_change_number":
//!   <int>,"depots":{"privatebranches":"1","branches":{"<name>":{"buildid":"<str>",
//!   "timeupdated":"<unix secs str>","timebuildupdated":"<unix secs str>",
//!   "description":"<str>"}}}}}}`. Numbers arrive as decimal strings (the app
//!   `_change_number` is a JSON integer); both forms are accepted. Only
//!   `public` and versioned public branches (`1.42.0.1`, `csgo`, ...) were
//!   present; **`staging`/`prerelease` were not visible** and
//!   `privatebranches` was `"1"`, so password-protected branches appear to be
//!   hidden from anonymous PICS. Whether they ever become visible through this
//!   proxy is UNVERIFIED. Rate limits and uptime of the proxy are UNVERIFIED.
//!   The `pwdrequired` field name is UNVERIFIED (taken from PICS conventions).
//! * [`SteamDbRssSource`] — `https://steamdb.info/api/PatchnotesRSS/?appid=730`.
//!   Observed on 2026-11-26 to be RSS 2.0 with `<guid>build#<buildid></guid>`
//!   and RFC 2822 `<pubDate>` per item. It lists **public builds only**. Whether
//!   SteamDB tolerates automated polling (Cloudflare, terms of use) is
//!   UNVERIFIED; treat it as a low-frequency fallback.
//!
//! All sources are read-only HTTP GETs; none can affect any marketplace.

use std::collections::{BTreeMap, VecDeque};
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use cs2_arb_core::{Error, Result, CS2_APP_ID};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Pseudo-branch name used for app-level PICS changelist bumps that are not
/// explained by any visible branch buildid change (see
/// [`crate::WatcherConfig::track_app_changelist`]).
pub const APP_CHANGELIST_BRANCH: &str = "_changelist";

const USER_AGENT: &str = "cs2-arb-mock/0.1 (read-only branch watcher)";
const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);
const DEFAULT_RETRY_AFTER: Duration = Duration::from_secs(60);

/// One branch's metadata as reported by a source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchInfo {
    pub buildid: u64,
    /// Steam `timeupdated` (unix seconds) when present.
    pub time_updated: Option<DateTime<Utc>>,
    /// Steam `timebuildupdated` (unix seconds) when present.
    pub time_build_updated: Option<DateTime<Utc>>,
    pub description: Option<String>,
    /// `pwdrequired` flag (UNVERIFIED field name).
    pub password_required: bool,
}

/// A point-in-time view of an app's branches.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct AppBranchSnapshot {
    pub app_id: u32,
    /// App-level PICS change number (`_change_number`), if the source reports it.
    pub change_number: Option<u64>,
    /// Whether the app reports that private branches exist (`privatebranches`).
    pub has_private_branches: bool,
    pub branches: BTreeMap<String, BranchInfo>,
}

/// A read-only source of branch metadata. Tests use [`ScriptedSource`].
#[async_trait]
pub trait BranchInfoSource: Send + Sync {
    /// Human-readable source name for logs.
    fn name(&self) -> &str;
    async fn fetch(&self) -> Result<AppBranchSnapshot>;
}

fn http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(DEFAULT_TIMEOUT)
        .build()
        .map_err(|err| Error::Network(err.to_string()))
}

async fn get_text(client: &reqwest::Client, url: &str) -> Result<String> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|err| Error::Network(err.without_url().to_string()))?;
    let status = response.status();
    if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
        let retry_after = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_RETRY_AFTER);
        return Err(Error::RateLimited { retry_after });
    }
    if !status.is_success() {
        return Err(Error::Network(format!("unexpected HTTP status {status}")));
    }
    response
        .text()
        .await
        .map_err(|err| Error::Network(err.without_url().to_string()))
}

/// Steam PICS product info via the public `api.steamcmd.net` JSON proxy.
#[derive(Debug, Clone)]
pub struct SteamCmdApiSource {
    client: reqwest::Client,
    base_url: String,
    app_id: u32,
}

impl SteamCmdApiSource {
    pub const DEFAULT_BASE_URL: &'static str = "https://api.steamcmd.net/v1/info";

    /// CS2 (730) against the public proxy.
    pub fn new() -> Result<Self> {
        Self::with_base_url(Self::DEFAULT_BASE_URL, CS2_APP_ID)
    }

    /// Custom base URL (no trailing app id), e.g. a local mock server in tests.
    pub fn with_base_url(base_url: impl Into<String>, app_id: u32) -> Result<Self> {
        Ok(Self::with_client(http_client()?, base_url, app_id))
    }

    pub fn with_client(client: reqwest::Client, base_url: impl Into<String>, app_id: u32) -> Self {
        Self {
            client,
            base_url: base_url.into().trim_end_matches('/').to_owned(),
            app_id,
        }
    }

    pub fn url(&self) -> String {
        format!("{}/{}", self.base_url, self.app_id)
    }
}

#[async_trait]
impl BranchInfoSource for SteamCmdApiSource {
    fn name(&self) -> &str {
        "steamcmd.net"
    }

    async fn fetch(&self) -> Result<AppBranchSnapshot> {
        let body = get_text(&self.client, &self.url()).await?;
        parse_steamcmd_info(&body, self.app_id)
    }
}

fn value_u64(value: &Value) -> Option<u64> {
    match value {
        Value::Number(number) => number.as_u64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

fn value_flag(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Bool(flag)) => *flag,
        Some(other) => value_u64(other).is_some_and(|n| n != 0),
        None => false,
    }
}

fn value_unix_time(value: Option<&Value>) -> Option<DateTime<Utc>> {
    value
        .and_then(value_u64)
        .and_then(|secs| i64::try_from(secs).ok())
        .and_then(|secs| DateTime::from_timestamp(secs, 0))
}

/// Parse an `api.steamcmd.net/v1/info/<app>` response body.
pub fn parse_steamcmd_info(body: &str, app_id: u32) -> Result<AppBranchSnapshot> {
    let root: Value = serde_json::from_str(body)
        .map_err(|err| Error::InvalidData(format!("steamcmd.net body is not JSON: {err}")))?;
    match root.get("status").and_then(Value::as_str) {
        Some("success") => {}
        other => {
            return Err(Error::InvalidData(format!(
                "steamcmd.net status is {other:?}, expected \"success\""
            )))
        }
    }
    let app_key = app_id.to_string();
    let app = root
        .get("data")
        .and_then(|data| data.get(&app_key))
        .filter(|app| app.as_object().is_some_and(|obj| !obj.is_empty()))
        .ok_or_else(|| Error::InvalidData(format!("steamcmd.net has no data for app {app_id}")))?;
    let depots = app
        .get("depots")
        .ok_or_else(|| Error::InvalidData("steamcmd.net app has no depots".into()))?;
    let branches_value = depots
        .get("branches")
        .and_then(Value::as_object)
        .ok_or_else(|| Error::InvalidData("steamcmd.net depots has no branches object".into()))?;

    let mut branches = BTreeMap::new();
    for (name, branch) in branches_value {
        let Some(buildid) = branch.get("buildid").and_then(value_u64) else {
            tracing::warn!(branch = %name, "skipping branch without a numeric buildid");
            continue;
        };
        branches.insert(
            name.clone(),
            BranchInfo {
                buildid,
                time_updated: value_unix_time(branch.get("timeupdated")),
                time_build_updated: value_unix_time(branch.get("timebuildupdated")),
                description: branch
                    .get("description")
                    .and_then(Value::as_str)
                    .map(|text| text.trim().to_owned()),
                password_required: value_flag(branch.get("pwdrequired")),
            },
        );
    }

    Ok(AppBranchSnapshot {
        app_id,
        change_number: app.get("_change_number").and_then(value_u64),
        has_private_branches: value_flag(depots.get("privatebranches")),
        branches,
    })
}

/// SteamDB patch-notes RSS. Public builds only; reported as branch `public`.
#[derive(Debug, Clone)]
pub struct SteamDbRssSource {
    client: reqwest::Client,
    url: String,
    app_id: u32,
}

impl SteamDbRssSource {
    pub const DEFAULT_URL_PREFIX: &'static str = "https://steamdb.info/api/PatchnotesRSS/?appid=";

    pub fn new() -> Result<Self> {
        Self::with_url(
            format!("{}{}", Self::DEFAULT_URL_PREFIX, CS2_APP_ID),
            CS2_APP_ID,
        )
    }

    pub fn with_url(url: impl Into<String>, app_id: u32) -> Result<Self> {
        Ok(Self {
            client: http_client()?,
            url: url.into(),
            app_id,
        })
    }
}

#[async_trait]
impl BranchInfoSource for SteamDbRssSource {
    fn name(&self) -> &str {
        "steamdb-rss"
    }

    async fn fetch(&self) -> Result<AppBranchSnapshot> {
        let body = get_text(&self.client, &self.url).await?;
        parse_steamdb_rss(&body, self.app_id)
    }
}

fn tag_text<'a>(block: &'a str, tag: &str) -> Option<&'a str> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let start = block.find(&open)?;
    let after_open = start + block[start..].find('>')? + 1;
    let end = after_open + block[after_open..].find(&close)?;
    Some(block[after_open..end].trim())
}

/// Parse SteamDB patch-notes RSS into a snapshot whose `public` branch is the
/// highest `build#<id>` item. Minimal parser for the observed feed shape.
pub fn parse_steamdb_rss(body: &str, app_id: u32) -> Result<AppBranchSnapshot> {
    let mut latest: Option<BranchInfo> = None;
    for item in body.split("<item>").skip(1) {
        let item = item.split("</item>").next().unwrap_or(item);
        let Some(buildid) = tag_text(item, "guid")
            .and_then(|guid| guid.strip_prefix("build#"))
            .and_then(|id| id.trim().parse::<u64>().ok())
        else {
            continue;
        };
        if latest.as_ref().is_some_and(|best| best.buildid >= buildid) {
            continue;
        }
        let published = tag_text(item, "pubDate")
            .and_then(|date| DateTime::parse_from_rfc2822(date).ok())
            .map(|date| date.with_timezone(&Utc));
        latest = Some(BranchInfo {
            buildid,
            time_updated: published,
            time_build_updated: published,
            description: tag_text(item, "title").map(str::to_owned),
            password_required: false,
        });
    }
    let latest =
        latest.ok_or_else(|| Error::InvalidData("SteamDB RSS contains no build# items".into()))?;
    Ok(AppBranchSnapshot {
        app_id,
        change_number: None,
        has_private_branches: false,
        branches: BTreeMap::from([("public".to_owned(), latest)]),
    })
}

/// A deterministic source that replays scripted responses, for tests,
/// fixtures and backtests. Returns an error once the script is exhausted.
#[derive(Debug, Default)]
pub struct ScriptedSource {
    script: Mutex<VecDeque<std::result::Result<AppBranchSnapshot, String>>>,
}

impl ScriptedSource {
    pub fn new(
        script: impl IntoIterator<Item = std::result::Result<AppBranchSnapshot, String>>,
    ) -> Self {
        Self {
            script: Mutex::new(script.into_iter().collect()),
        }
    }

    pub fn from_snapshots(snapshots: impl IntoIterator<Item = AppBranchSnapshot>) -> Self {
        Self::new(snapshots.into_iter().map(Ok))
    }

    pub fn push(&self, step: std::result::Result<AppBranchSnapshot, String>) {
        self.lock().push_back(step);
    }

    pub fn remaining(&self) -> usize {
        self.lock().len()
    }

    fn lock(
        &self,
    ) -> std::sync::MutexGuard<'_, VecDeque<std::result::Result<AppBranchSnapshot, String>>> {
        self.script
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }
}

#[async_trait]
impl BranchInfoSource for ScriptedSource {
    fn name(&self) -> &str {
        "scripted"
    }

    async fn fetch(&self) -> Result<AppBranchSnapshot> {
        match self.lock().pop_front() {
            Some(Ok(snapshot)) => Ok(snapshot),
            Some(Err(message)) => Err(Error::Network(message)),
            None => Err(Error::Network("scripted source exhausted".into())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_numbers_as_strings_or_integers() {
        let body = r#"{"status":"success","data":{"730":{"_change_number":"5",
            "depots":{"privatebranches":1,"branches":{
              "public":{"buildid":10,"timeupdated":"1700000000"},
              "staging":{"buildid":"11","pwdrequired":"1","description":" dev "},
              "broken":{"description":"no build id"}}}}}}"#;
        let snapshot = parse_steamcmd_info(body, 730).unwrap();
        assert_eq!(snapshot.change_number, Some(5));
        assert!(snapshot.has_private_branches);
        assert_eq!(snapshot.branches.len(), 2);
        assert_eq!(snapshot.branches["public"].buildid, 10);
        assert_eq!(
            snapshot.branches["public"].time_updated,
            DateTime::from_timestamp(1_700_000_000, 0)
        );
        let staging = &snapshot.branches["staging"];
        assert!(staging.password_required);
        assert_eq!(staging.description.as_deref(), Some("dev"));
    }

    #[test]
    fn rejects_failure_status_and_missing_app() {
        assert!(parse_steamcmd_info(r#"{"status":"failed","data":{}}"#, 730).is_err());
        assert!(parse_steamcmd_info(r#"{"status":"success","data":{"730":{}}}"#, 730).is_err());
        assert!(parse_steamcmd_info("not json", 730).is_err());
    }
}
