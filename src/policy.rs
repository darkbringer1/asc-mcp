//! Per-profile tool access (OMB fork).
//!
//! Every tool is compiled in; a profile opens the ones a caller needs. One
//! process serves one profile (`ASC_PROFILE`), read from a JSON file
//! (`ASC_PROFILES`, default `/etc/asc-mcp/profiles.json`) that is re-read when
//! it changes, so opening or closing a tool needs no restart.
//!
//! Closed is the default: a tool nobody opened is not listed and not callable.
//! A missing or broken file, or a profile that isn't in it, closes everything.
//!
//! Each tool has a [`Tier`], derived from its group and verb:
//!
//! - `public`: public App Store data, no API key (the market group).
//! - `read`: reads the account.
//! - `write`: changes that stay in App Store Connect until someone ships them
//!   (metadata, screenshots, review replies, TestFlight notes).
//! - `release`: reaches customers or money (review submission, phased release,
//!   pricing, availability, in-app purchases, subscriptions, offers).
//! - `danger`: removes things, or touches users, signing or webhooks.
//!
//! Selectors in a profile are a tool name, `group:<group>` or `tier:<tier>`.
//! Group and tier selectors never open a `danger` tool: that takes the exact
//! tool name with `"dangerous": true`. `release` and `danger` calls (and any
//! non-GET `api_execute`) are not run directly: the call is stored and the
//! caller gets a token for `confirm_action`, unless the profile sets
//! `"confirm": false`.
//!
//! The generic `api_*` tools are opened by operation, not by name: an `api`
//! grant names an HTTP method and a path pattern (`*` is one segment, a final
//! `**` is the rest). `DELETE` grants need `"dangerous": true`.

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::server::catalog::{Effect, Group};

/// Default location of the profiles file.
pub const DEFAULT_PROFILES_PATH: &str = "/etc/asc-mcp/profiles.json";

/// How long a stored `release`/`danger` call waits for `confirm_action`.
pub const PENDING_TTL_SECS: u64 = 600;

/// How far a tool reaches, from public data to irreversible changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Public,
    Read,
    Write,
    Release,
    Danger,
}

impl Tier {
    pub const ALL: [Tier; 5] = [Tier::Public, Tier::Read, Tier::Write, Tier::Release, Tier::Danger];

    pub fn as_str(self) -> &'static str {
        match self {
            Tier::Public => "public",
            Tier::Read => "read",
            Tier::Write => "write",
            Tier::Release => "release",
            Tier::Danger => "danger",
        }
    }

    pub fn parse(raw: &str) -> Option<Tier> {
        let raw = raw.trim().to_ascii_lowercase();
        Tier::ALL.into_iter().find(|t| t.as_str() == raw)
    }

    /// Whether a call at this tier is stored for `confirm_action` first.
    pub fn needs_confirm(self) -> bool {
        matches!(self, Tier::Release | Tier::Danger)
    }
}

/// Groups whose writes reach customers or money.
const RELEASE_GROUPS: &[Group] = &[
    Group::Submission,
    Group::Pricing,
    Group::Availability,
    Group::Iap,
    Group::Subscriptions,
    Group::Offers,
    Group::OfferCodes,
    Group::Promotions,
];

/// Groups whose writes change who or what can act on the account.
const DANGER_GROUPS: &[Group] = &[Group::Users, Group::Provisioning, Group::Webhooks];

/// Release-tier writes that live in otherwise ordinary groups.
const RELEASE_TOOLS: &[&str] = &["start_phased_release", "update_phased_release"];

/// The tier of a tool, from its name, group and effect. Unknown effects are
/// `danger`: an unclassified tool might do anything.
pub fn tier_of(name: &str, group: Group, effect: Option<Effect>) -> Tier {
    let Some(effect) = effect else {
        return Tier::Danger;
    };
    if group == Group::Market {
        return Tier::Public;
    }
    match effect {
        Effect::Read => Tier::Read,
        Effect::Destructive | Effect::MethodDependent => Tier::Danger,
        Effect::Create | Effect::Update => {
            if DANGER_GROUPS.contains(&group) {
                Tier::Danger
            } else if RELEASE_GROUPS.contains(&group)
                || RELEASE_TOOLS.contains(&name)
                || name.starts_with("submit_")
            {
                Tier::Release
            } else {
                Tier::Write
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The profiles file
// ---------------------------------------------------------------------------

/// The whole profiles file.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProfileFile {
    #[serde(default)]
    pub profiles: BTreeMap<String, Profile>,
}

/// What one caller may use.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Profile {
    /// Free text for the owner; never shown to the caller.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Tools, groups or tiers opened for this profile.
    #[serde(default)]
    pub open: Vec<Grant>,
    /// Selectors closed even when an `open` entry matches.
    #[serde(default)]
    pub deny: Vec<String>,
    /// Operations opened through `api_execute` / `api_list`.
    #[serde(default)]
    pub api: Vec<ApiGrant>,
    /// Store `release`/`danger` calls for `confirm_action` (default true).
    #[serde(default = "yes")]
    pub confirm: bool,
}

impl Default for Profile {
    fn default() -> Self {
        Self {
            note: None,
            open: Vec::new(),
            deny: Vec::new(),
            api: Vec::new(),
            confirm: true,
        }
    }
}

fn yes() -> bool {
    true
}

/// One opened selector.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Grant {
    /// A tool name, `group:<group>` or `tier:<tier>`.
    pub select: String,
    /// UTC expiry, `YYYY-MM-DDTHH:MM:SSZ`. Absent means until closed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Required to open a `danger` tool (by exact name only).
    #[serde(default, skip_serializing_if = "is_false")]
    pub dangerous: bool,
}

/// One opened API operation pattern.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApiGrant {
    /// HTTP method: GET, POST, PATCH, PUT or DELETE.
    pub method: String,
    /// Path pattern, e.g. `/v1/apps/*/customerReviews` or `/v1/apps/**`.
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub until: Option<String>,
    /// Required for DELETE.
    #[serde(default, skip_serializing_if = "is_false")]
    pub dangerous: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// What a profile allows for one tool or operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Closed,
    Open {
        /// Store the call for `confirm_action` instead of running it.
        confirm: bool,
        /// Unix seconds the grant ends; `None` means until closed.
        until: Option<u64>,
    },
}

impl Access {
    pub fn is_open(self) -> bool {
        matches!(self, Access::Open { .. })
    }
}

/// What a selector names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selector {
    Tool(String),
    Group(Group),
    Tier(Tier),
}

impl Selector {
    pub fn parse(raw: &str) -> Option<Selector> {
        let raw = raw.trim();
        if let Some(g) = raw.strip_prefix("group:") {
            return Group::parse(g).map(Selector::Group);
        }
        if let Some(t) = raw.strip_prefix("tier:") {
            return Tier::parse(t).map(Selector::Tier);
        }
        if raw.is_empty() || raw.contains(':') {
            return None;
        }
        Some(Selector::Tool(raw.to_string()))
    }

    /// Whether this selector names the tool. Group and tier selectors never
    /// name a `danger` tool.
    fn names(&self, name: &str, group: Group, tier: Tier) -> bool {
        match self {
            Selector::Tool(t) => t == name,
            Selector::Group(g) => *g == group && tier != Tier::Danger,
            Selector::Tier(t) => *t == tier && tier != Tier::Danger,
        }
    }
}

/// Merge one matching grant's expiry into the running result: open-ended wins,
/// otherwise the latest expiry.
fn merge_until(acc: Option<Option<u64>>, until: Option<u64>) -> Option<Option<u64>> {
    match (acc, until) {
        (None, u) => Some(u),
        (Some(None), _) | (_, None) => Some(None),
        (Some(Some(a)), Some(b)) => Some(Some(a.max(b))),
    }
}

/// Parse a grant's `until`. `Err` (bad format) is treated as expired by callers.
fn grant_until(raw: &Option<String>) -> Result<Option<u64>, ()> {
    match raw {
        None => Ok(None),
        Some(s) => parse_utc(s).map(Some).ok_or(()),
    }
}

impl Profile {
    /// Access to a curated tool.
    pub fn tool_access(&self, name: &str, group: Group, tier: Tier, now: u64) -> Access {
        if self.deny.iter().any(|d| {
            Selector::parse(d).is_some_and(|s| match s {
                // A deny by group or tier also covers danger tools.
                Selector::Tool(t) => t == name,
                Selector::Group(g) => g == group,
                Selector::Tier(t) => t == tier,
            })
        }) {
            return Access::Closed;
        }

        let mut open: Option<Option<u64>> = None;
        for grant in &self.open {
            let Some(selector) = Selector::parse(&grant.select) else {
                continue;
            };
            if !selector.names(name, group, tier) {
                continue;
            }
            if tier == Tier::Danger && !grant.dangerous {
                continue;
            }
            let Ok(until) = grant_until(&grant.until) else {
                continue;
            };
            if until.is_some_and(|u| u <= now) {
                continue;
            }
            open = merge_until(open, until);
        }

        match open {
            None => Access::Closed,
            Some(until) => Access::Open {
                confirm: self.confirm && tier.needs_confirm(),
                until,
            },
        }
    }

    /// Access to one operation through `api_execute` / `api_list`.
    pub fn api_access(&self, method: &str, raw_path: &str, now: u64) -> Access {
        let method = method.trim().to_ascii_uppercase();
        let Some(path) = normalize_api_path(raw_path) else {
            return Access::Closed;
        };
        let mut open: Option<Option<u64>> = None;
        for grant in &self.api {
            if !grant.method.trim().eq_ignore_ascii_case(&method) {
                continue;
            }
            if method == "DELETE" && !grant.dangerous {
                continue;
            }
            if !path_matches(&grant.path, &path) {
                continue;
            }
            let Ok(until) = grant_until(&grant.until) else {
                continue;
            };
            if until.is_some_and(|u| u <= now) {
                continue;
            }
            open = merge_until(open, until);
        }
        match open {
            None => Access::Closed,
            Some(until) => Access::Open {
                confirm: self.confirm && method != "GET",
                until,
            },
        }
    }

    /// Whether any API grant is live (opens `api_search` / `api_describe`).
    pub fn has_api(&self, now: u64) -> bool {
        self.api
            .iter()
            .any(|g| matches!(grant_until(&g.until), Ok(u) if u.is_none_or(|u| u > now)))
    }

    /// Whether any live API grant is for `method`.
    pub fn has_api_method(&self, method: &str, now: u64) -> bool {
        self.api.iter().any(|g| {
            g.method.trim().eq_ignore_ascii_case(method)
                && matches!(grant_until(&g.until), Ok(u) if u.is_none_or(|u| u > now))
        })
    }
}

/// Reduce a path or same-host URL to `/v1/...` for matching. Anything odd
/// (dot segments, percent-escapes, empty segments, backslashes) closes.
pub fn normalize_api_path(raw: &str) -> Option<String> {
    let raw = raw.trim();
    let path = if raw.starts_with("http://") || raw.starts_with("https://") {
        let url = reqwest::Url::parse(raw).ok()?;
        url.path().to_string()
    } else {
        let no_query = raw.split(['?', '#']).next().unwrap_or("");
        format!("/{}", no_query.trim_start_matches('/'))
    };
    if path.contains('%') || path.contains('\\') || path.contains("//") {
        return None;
    }
    let segments: Vec<&str> = path.trim_start_matches('/').split('/').collect();
    if segments.iter().any(|s| s.is_empty() || *s == "." || *s == "..") {
        return None;
    }
    Some(path.trim_end_matches('/').to_string())
}

/// Match a normalized path against a pattern: `*` is exactly one segment, a
/// final `**` is one or more segments, anything else must be equal.
pub fn path_matches(pattern: &str, path: &str) -> bool {
    let pat: Vec<&str> = pattern
        .trim()
        .trim_matches('/')
        .split('/')
        .filter(|s| !s.is_empty())
        .collect();
    let segs: Vec<&str> = path.trim_matches('/').split('/').collect();
    for (i, p) in pat.iter().enumerate() {
        if *p == "**" {
            return i == pat.len() - 1 && segs.len() > i;
        }
        let Some(s) = segs.get(i) else {
            return false;
        };
        if *p != "*" && p != s {
            return false;
        }
    }
    pat.len() == segs.len()
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// The profile this process serves, re-read from disk when the file changes.
pub struct Policy {
    path: PathBuf,
    profile: String,
    cache: Mutex<Option<Cached>>,
}

struct Cached {
    stamp: Option<(SystemTime, u64)>,
    profile: Profile,
}

impl Policy {
    /// `Some` when `ASC_PROFILE` is set. The file comes from `ASC_PROFILES`.
    pub fn from_env() -> Option<Policy> {
        let profile = std::env::var("ASC_PROFILE")
            .ok()
            .map(|p| p.trim().to_string())
            .filter(|p| !p.is_empty())?;
        let path = std::env::var("ASC_PROFILES")
            .ok()
            .filter(|p| !p.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_PROFILES_PATH.to_string());
        Some(Policy::new(PathBuf::from(path), profile))
    }

    pub fn new(path: PathBuf, profile: String) -> Policy {
        Policy {
            path,
            profile,
            cache: Mutex::new(None),
        }
    }

    pub fn profile_name(&self) -> &str {
        &self.profile
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The current profile. Any problem closes everything and is logged.
    pub fn current(&self) -> Profile {
        let stamp = std::fs::metadata(&self.path)
            .ok()
            .and_then(|m| Some((m.modified().ok()?, m.len())));
        let mut cache = self.cache.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(c) = cache.as_ref() {
            if c.stamp.is_some() && c.stamp == stamp {
                return c.profile.clone();
            }
        }
        let profile = match load_file(&self.path) {
            Ok(file) => match file.profiles.get(&self.profile) {
                Some(p) => p.clone(),
                None => {
                    tracing::error!(
                        profile = %self.profile,
                        file = %self.path.display(),
                        "profile not in the profiles file; every tool is closed"
                    );
                    Profile::default()
                }
            },
            Err(e) => {
                tracing::error!(file = %self.path.display(), "{e}; every tool is closed");
                Profile::default()
            }
        };
        *cache = Some(Cached {
            stamp,
            profile: profile.clone(),
        });
        profile
    }
}

/// Read and parse a profiles file.
pub fn load_file(path: &Path) -> Result<ProfileFile, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("cannot read profiles file {}: {e}", path.display()))?;
    serde_json::from_str(&text)
        .map_err(|e| format!("cannot parse profiles file {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Pending release/danger calls
// ---------------------------------------------------------------------------

/// A call waiting for `confirm_action`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PendingCall {
    pub profile: String,
    pub tool: String,
    #[serde(default)]
    pub arguments: Option<Map<String, Value>>,
    pub created: u64,
    pub expires: u64,
}

/// Pending calls, one file each, so a restarted process still finds them.
pub struct PendingStore {
    dir: PathBuf,
}

impl PendingStore {
    /// `ASC_STATE_DIR`, else `$XDG_STATE_HOME/asc-mcp`, else
    /// `$HOME/.local/state/asc-mcp`; calls live in its `pending/`.
    pub fn from_env() -> PendingStore {
        let base = std::env::var("ASC_STATE_DIR")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .or_else(|| {
                std::env::var("XDG_STATE_HOME")
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .map(|s| PathBuf::from(s).join("asc-mcp"))
            })
            .or_else(|| {
                std::env::var("HOME")
                    .ok()
                    .map(|h| PathBuf::from(h).join(".local/state/asc-mcp"))
            })
            .unwrap_or_else(|| std::env::temp_dir().join("asc-mcp"));
        PendingStore::new(base.join("pending"))
    }

    pub fn new(dir: PathBuf) -> PendingStore {
        PendingStore { dir }
    }

    /// Store a call; returns its token.
    pub fn put(&self, call: &PendingCall) -> Result<String, String> {
        create_private_dir(&self.dir)?;
        self.sweep(call.created);
        let token = random_token()?;
        let path = self.dir.join(format!("{token}.json"));
        let text = serde_json::to_string(call).map_err(|e| e.to_string())?;
        write_private(&path, text.as_bytes())?;
        Ok(token)
    }

    /// Remove and return a call. Single use: a second take fails.
    pub fn take(&self, token: &str, now: u64) -> Result<PendingCall, String> {
        let token = token.trim();
        if token.len() != 32 || !token.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err("unknown or expired token".into());
        }
        let path = self.dir.join(format!("{}.json", token.to_ascii_lowercase()));
        let text = std::fs::read_to_string(&path).map_err(|_| "unknown or expired token".to_string())?;
        std::fs::remove_file(&path).map_err(|e| format!("cannot consume token: {e}"))?;
        let call: PendingCall =
            serde_json::from_str(&text).map_err(|_| "unknown or expired token".to_string())?;
        if call.expires <= now {
            return Err("token expired; call the tool again for a new one".into());
        }
        Ok(call)
    }

    /// Delete expired calls.
    fn sweep(&self, now: u64) {
        let Ok(entries) = std::fs::read_dir(&self.dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let expired = std::fs::read_to_string(&path)
                .ok()
                .and_then(|t| serde_json::from_str::<PendingCall>(&t).ok())
                .is_none_or(|c| c.expires <= now);
            if expired {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn random_token() -> Result<String, String> {
    let mut buf = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut buf))
        .map_err(|e| format!("cannot read /dev/urandom: {e}"))?;
    Ok(hex::encode(buf))
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    if dir.is_dir() {
        return Ok(());
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))
}

#[cfg(unix)]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))?;
    f.write_all(bytes)
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

#[cfg(not(unix))]
fn write_private(path: &Path, bytes: &[u8]) -> Result<(), String> {
    std::fs::write(path, bytes).map_err(|e| format!("cannot write {}: {e}", path.display()))
}

// ---------------------------------------------------------------------------
// Time (UTC, `YYYY-MM-DDTHH:MM:SSZ`, no dependency)
// ---------------------------------------------------------------------------

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Days since 1970-01-01 for a civil date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Parse `YYYY-MM-DDTHH:MM:SSZ` (or `YYYY-MM-DDTHH:MMZ`) as UTC.
pub fn parse_utc(raw: &str) -> Option<u64> {
    let raw = raw.trim().strip_suffix('Z')?;
    let (date, time) = raw.split_once('T')?;
    let mut d = date.split('-');
    let (y, m, day): (i64, i64, i64) = (
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
        d.next()?.parse().ok()?,
    );
    if d.next().is_some() {
        return None;
    }
    let mut t = time.split(':');
    let h: i64 = t.next()?.parse().ok()?;
    let min: i64 = t.next()?.parse().ok()?;
    let s: i64 = match t.next() {
        Some(s) => s.parse().ok()?,
        None => 0,
    };
    if t.next().is_some()
        || !(1970..=9999).contains(&y)
        || !(1..=12).contains(&m)
        || !(1..=31).contains(&day)
        || !(0..24).contains(&h)
        || !(0..60).contains(&min)
        || !(0..60).contains(&s)
    {
        return None;
    }
    let secs = days_from_civil(y, m, day) * 86400 + h * 3600 + min * 60 + s;
    u64::try_from(secs).ok()
}

pub fn format_utc(secs: u64) -> String {
    let days = (secs / 86400) as i64;
    let rem = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        (rem % 3600) / 60,
        rem % 60
    )
}

/// Parse a duration like `90m`, `2h`, `7d` or `45s` into seconds.
pub fn parse_duration(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    let (num, unit) = raw.split_at(raw.find(|c: char| !c.is_ascii_digit())?);
    let n: u64 = num.parse().ok()?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        _ => return None,
    };
    n.checked_mul(mult).filter(|s| *s > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    const NOW: u64 = 1_800_000_000;

    fn profile(json: Value) -> Profile {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn tiers_follow_group_and_verb() {
        assert_eq!(tier_of("search_store_apps", Group::Market, Some(Effect::Read)), Tier::Public);
        assert_eq!(tier_of("list_apps", Group::Apps, Some(Effect::Read)), Tier::Read);
        assert_eq!(tier_of("list_users", Group::Users, Some(Effect::Read)), Tier::Read);
        assert_eq!(tier_of("respond_to_review", Group::Reviews, Some(Effect::Create)), Tier::Write);
        assert_eq!(
            tier_of("update_version_localization", Group::Versions, Some(Effect::Update)),
            Tier::Write
        );
        assert_eq!(tier_of("start_phased_release", Group::Versions, Some(Effect::Create)), Tier::Release);
        assert_eq!(
            tier_of("submit_build_for_beta_review", Group::TestFlight, Some(Effect::Create)),
            Tier::Release
        );
        assert_eq!(tier_of("set_subscription_price", Group::Subscriptions, Some(Effect::Update)), Tier::Release);
        assert_eq!(tier_of("invite_user", Group::Users, Some(Effect::Create)), Tier::Danger);
        assert_eq!(tier_of("create_webhook", Group::Webhooks, Some(Effect::Create)), Tier::Danger);
        assert_eq!(tier_of("delete_review_response", Group::Reviews, Some(Effect::Destructive)), Tier::Danger);
        assert_eq!(tier_of("api_execute", Group::Generic, Some(Effect::MethodDependent)), Tier::Danger);
        assert_eq!(tier_of("frobnicate", Group::Apps, None), Tier::Danger);
    }

    #[test]
    fn empty_profile_closes_everything() {
        let p = Profile::default();
        assert_eq!(p.tool_access("list_apps", Group::Apps, Tier::Read, NOW), Access::Closed);
        assert_eq!(p.api_access("GET", "/v1/apps", NOW), Access::Closed);
        assert!(!p.has_api(NOW));
    }

    #[test]
    fn tier_group_and_name_selectors_open() {
        let p = profile(serde_json::json!({
            "open": [{"select": "tier:read"}, {"select": "group:reviews"}, {"select": "start_phased_release"}]
        }));
        assert!(p.tool_access("list_apps", Group::Apps, Tier::Read, NOW).is_open());
        assert!(p.tool_access("respond_to_review", Group::Reviews, Tier::Write, NOW).is_open());
        assert_eq!(
            p.tool_access("start_phased_release", Group::Versions, Tier::Release, NOW),
            Access::Open { confirm: true, until: None }
        );
        assert_eq!(
            p.tool_access("update_app", Group::Apps, Tier::Write, NOW),
            Access::Closed
        );
    }

    #[test]
    fn danger_needs_exact_name_and_flag() {
        let by_group = profile(serde_json::json!({"open": [{"select": "group:reviews", "dangerous": true}]}));
        assert_eq!(
            by_group.tool_access("delete_review_response", Group::Reviews, Tier::Danger, NOW),
            Access::Closed
        );
        let no_flag = profile(serde_json::json!({"open": [{"select": "delete_review_response"}]}));
        assert_eq!(
            no_flag.tool_access("delete_review_response", Group::Reviews, Tier::Danger, NOW),
            Access::Closed
        );
        let ok = profile(serde_json::json!({"open": [{"select": "delete_review_response", "dangerous": true}]}));
        assert_eq!(
            ok.tool_access("delete_review_response", Group::Reviews, Tier::Danger, NOW),
            Access::Open { confirm: true, until: None }
        );
        let tier = profile(serde_json::json!({"open": [{"select": "tier:danger", "dangerous": true}]}));
        assert_eq!(
            tier.tool_access("remove_user", Group::Users, Tier::Danger, NOW),
            Access::Closed
        );
    }

    #[test]
    fn deny_wins_over_open() {
        let p = profile(serde_json::json!({
            "open": [{"select": "tier:read"}],
            "deny": ["download_finance_report", "group:users"]
        }));
        assert_eq!(
            p.tool_access("download_finance_report", Group::Analytics, Tier::Read, NOW),
            Access::Closed
        );
        assert_eq!(p.tool_access("list_users", Group::Users, Tier::Read, NOW), Access::Closed);
        assert!(p.tool_access("list_apps", Group::Apps, Tier::Read, NOW).is_open());
    }

    #[test]
    fn expiry_closes_and_bad_dates_fail_closed() {
        let p = profile(serde_json::json!({"open": [
            {"select": "list_apps", "until": format_utc(NOW - 1)},
            {"select": "get_app", "until": format_utc(NOW + 60)},
            {"select": "list_builds", "until": "tomorrow"}
        ]}));
        assert_eq!(p.tool_access("list_apps", Group::Apps, Tier::Read, NOW), Access::Closed);
        assert_eq!(
            p.tool_access("get_app", Group::Apps, Tier::Read, NOW),
            Access::Open { confirm: false, until: Some(NOW + 60) }
        );
        assert_eq!(p.tool_access("list_builds", Group::TestFlight, Tier::Read, NOW), Access::Closed);
    }

    #[test]
    fn confirm_can_be_switched_off() {
        let p = profile(serde_json::json!({"confirm": false, "open": [{"select": "tier:release"}]}));
        assert_eq!(
            p.tool_access("start_phased_release", Group::Versions, Tier::Release, NOW),
            Access::Open { confirm: false, until: None }
        );
    }

    #[test]
    fn api_grants_match_method_and_pattern() {
        let p = profile(serde_json::json!({"api": [
            {"method": "GET", "path": "/v1/apps/*/customerReviews"},
            {"method": "PATCH", "path": "/v1/appStoreVersionLocalizations/*"},
            {"method": "GET", "path": "/v1/salesReports/**"},
            {"method": "DELETE", "path": "/v1/customerReviewResponses/*"}
        ]}));
        assert_eq!(
            p.api_access("get", "/v1/apps/123/customerReviews", NOW),
            Access::Open { confirm: false, until: None }
        );
        assert_eq!(
            p.api_access("GET", "https://api.appstoreconnect.apple.com/v1/apps/123/customerReviews?cursor=abc", NOW),
            Access::Open { confirm: false, until: None }
        );
        assert_eq!(p.api_access("GET", "/v1/apps/123", NOW), Access::Closed);
        assert_eq!(p.api_access("POST", "/v1/apps/123/customerReviews", NOW), Access::Closed);
        assert_eq!(
            p.api_access("PATCH", "v1/appStoreVersionLocalizations/9", NOW),
            Access::Open { confirm: true, until: None }
        );
        assert!(p.api_access("GET", "/v1/salesReports/a/b", NOW).is_open());
        assert_eq!(p.api_access("GET", "/v1/salesReports", NOW), Access::Closed);
        // DELETE without the flag stays closed.
        assert_eq!(p.api_access("DELETE", "/v1/customerReviewResponses/1", NOW), Access::Closed);
    }

    #[test]
    fn odd_paths_close() {
        let p = profile(serde_json::json!({"api": [{"method": "GET", "path": "/v1/**"}]}));
        for path in [
            "/v1/apps/../users",
            "/v1/apps/%2e%2e/users",
            "/v1//users",
            "/v1/apps/./x",
            "https://api.appstoreconnect.apple.com/v1/apps/..%2Fusers",
        ] {
            assert_eq!(p.api_access("GET", path, NOW), Access::Closed, "{path}");
        }
        assert!(p.api_access("GET", "/v1/users", NOW).is_open());
    }

    #[test]
    fn selectors_parse() {
        assert_eq!(Selector::parse("tier:read"), Some(Selector::Tier(Tier::Read)));
        assert_eq!(Selector::parse("group:offer_codes"), Some(Selector::Group(Group::OfferCodes)));
        assert_eq!(Selector::parse("list_apps"), Some(Selector::Tool("list_apps".into())));
        assert_eq!(Selector::parse("group:nope"), None);
        assert_eq!(Selector::parse("x:y"), None);
    }

    #[test]
    fn time_round_trips() {
        for s in ["1970-01-01T00:00:00Z", "2026-11-20T18:00:00Z", "2028-02-29T23:59:59Z"] {
            assert_eq!(format_utc(parse_utc(s).unwrap()), s);
        }
        assert_eq!(parse_utc("2026-11-20T18:00Z"), parse_utc("2026-11-20T18:00:00Z"));
        assert_eq!(parse_utc("2026-11-20T18:00:00+01:00"), None);
        assert_eq!(parse_utc("2026-13-01T00:00:00Z"), None);
        assert_eq!(parse_duration("2h"), Some(7200));
        assert_eq!(parse_duration("7d"), Some(604800));
        assert_eq!(parse_duration("0h"), None);
        assert_eq!(parse_duration("2w"), None);
    }

    #[test]
    fn pending_is_single_use_and_expires() {
        let dir = std::env::temp_dir().join(format!("asc-mcp-test-{}", random_token().unwrap()));
        let store = PendingStore::new(dir.clone());
        let call = PendingCall {
            profile: "p".into(),
            tool: "start_phased_release".into(),
            arguments: None,
            created: NOW,
            expires: NOW + PENDING_TTL_SECS,
        };
        let token = store.put(&call).unwrap();
        assert_eq!(store.take(&token, NOW).unwrap().tool, "start_phased_release");
        assert!(store.take(&token, NOW).is_err(), "second take must fail");

        let token = store.put(&call).unwrap();
        assert!(store.take(&token, NOW + PENDING_TTL_SECS).is_err(), "expired");
        assert!(store.take("../../etc/passwd", NOW).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn unknown_fields_are_rejected() {
        let bad: Result<ProfileFile, _> =
            serde_json::from_value(serde_json::json!({"profiles": {"a": {"opne": []}}}));
        assert!(bad.is_err());
    }
}
