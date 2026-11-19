//! The MCP server: the [`AppStoreServer`] type, its combined tool router, and
//! shared helpers used by every tool module.
//!
//! Tools are organized by domain into submodules, each contributing a
//! `#[tool_router(router = <name>_router)]` impl block. [`AppStoreServer::new`]
//! sums those routers into the single [`ToolRouter`] stored on the struct, and
//! `#[tool_handler(router = self.tool_router)]` dispatches off it.

mod access;
mod analytics;
mod apps;
mod asset_library;
mod assets;
mod availability;
pub mod catalog;
mod custom_product_pages;
mod discovery;
mod events;
mod generic;
mod iap;
pub(crate) mod market;
mod offer_codes;
mod offers;
mod pricing;
mod promotions;
mod provisioning;
mod reviews;
mod submission;
mod subscriptions;
mod testflight;
mod users;
mod versions;
mod webhooks;
mod xcode_cloud;

use std::collections::BTreeMap;
use std::sync::Arc;

use rmcp::{
    handler::server::router::tool::ToolRouter, model::*, tool_handler, ErrorData as McpError,
    ServerHandler,
};
use serde::Deserialize;
use serde_json::Value;

use crate::client::AscClient;
use crate::config::Config;
use crate::error::AscError;
use crate::policy::{self, Access, PendingCall, PendingStore, Policy, Profile, Tier};
use catalog::Group;

/// Server instructions shown to MCP clients to orient the agent.
const INSTRUCTIONS: &str = "\
asc-mcp wraps the Apple App Store Connect API, plus public App Store data.

Credentials come from the environment (ASC_ISSUER_ID, ASC_KEY_ID, and either \
ASC_PRIVATE_KEY or ASC_PRIVATE_KEY_PATH). If they are unset, App Store Connect \
tools return a configuration error; the market tools still work.

Three layers:
- Curated tools for the common jobs: apps & metadata, in-app purchases, \
  subscriptions and their offers, versions, pricing, availability, App Review \
  submission, TestFlight, provisioning, the App Asset Library (screenshots, \
  previews, product page headers, search result assets), custom product pages, \
  in-app events, promoted purchases, customer reviews, users, Xcode Cloud, \
  analytics, sales and finance reports, and webhooks.
- The full API: api_search finds any of Apple's ~1,300 operations by words \
  (Game Center, App Clips, background assets, alternative distribution, \
  product page optimization, anything without a curated tool), api_describe \
  shows its parameters and body, and api_execute calls it after checking the \
  request against Apple's spec. api_list pages through any collection.
- Market tools (search_store_apps, get_store_app, list_store_reviews, \
  analyze_store_keyword, search_suggestions, ...) read the public App Store \
  for any app, competitors included, with no API key.

Tips:
- IDs are opaque strings returned by list/get tools; resolve them first.
- Pricing requires a price-point ID: use the pricing tools to look them up.
- Write bodies are JSON:API: \
  {\"data\": {\"type\": ..., \"attributes\": {...}, \"relationships\": {...}}}. \
  When api_execute rejects a request, its error lists what Apple accepts.
- api_list can walk pages for you: pass `max_pages` instead of calling it \
  again with each `cursor`.
- Responses are trimmed to fit a context budget. A `_truncated` key means items \
  were dropped; narrow the query with `limit`/`filter[...]`/`fields[...]`.
- In-app purchases and subscriptions are submitted for review as their \
  versions (add_review_submission_item with inAppPurchaseVersion / \
  subscriptionVersion).

Limitations (enforced by Apple, not this server):
- New apps cannot be created via the API. Create the app on the App Store \
  Connect website first; you can pre-create its bundle ID with create_bundle_id.
- App Privacy labels, agreements, tax and banking have no API.";

const DISCOVERY_INSTRUCTIONS: &str = "\
This server wraps the Apple App Store Connect API in discovery mode. Use \
search_tools to find operations, get_tool_details to inspect an operation's \
input schema and safety annotations, then call_discovered_tool with its exact \
name and arguments. call_discovered_tool can write or delete account data; \
check the inspected annotations and obtain approval before writes. \
ASC_TOOLS and ASC_READ_ONLY still restrict which operations are available. \
Credentials come from ASC_ISSUER_ID, ASC_KEY_ID, and ASC_PRIVATE_KEY or \
ASC_PRIVATE_KEY_PATH. A _truncated response means narrow the request.";

/// The App Store Connect MCP server.
#[derive(Clone)]
pub struct AppStoreServer {
    pub(crate) client: Arc<AscClient>,
    /// Public App Store endpoints, which need no API key (see [`market`]).
    pub(crate) market: Arc<market::MarketClient>,
    tool_router: ToolRouter<AppStoreServer>,
    discovery_router: Option<ToolRouter<AppStoreServer>>,
    /// OMB fork: the caller's profile. `None` serves every tool (upstream
    /// behaviour); `Some` closes every tool the profile doesn't open.
    pub(crate) policy: Option<Arc<Policy>>,
    /// Held `release`/`danger` calls waiting for `confirm_action`.
    pub(crate) pending: Arc<PendingStore>,
    /// Every domain tool's group and tier, by name.
    pub(crate) index: Arc<BTreeMap<String, (Group, Tier)>>,
}

impl AppStoreServer {
    /// Build the server from configuration, assembling the per-domain routers
    /// into the subset this configuration serves (see [`catalog`]).
    pub fn new(config: Config) -> Self {
        let tools = config.tools.clone();
        let assembled = catalog::assemble(Self::domain_routers(), &tools);

        for warning in &assembled.warnings {
            tracing::warn!("{warning}");
        }
        if !assembled.unclassified.is_empty() {
            tracing::error!(
                tools = ?assembled.unclassified,
                "these tools' names start with a verb `server::catalog::VERBS` doesn't know, so \
                 they are served without safety annotations; classify them there"
            );
        }
        let domain_tool_count = assembled.router.map.len();
        let (tool_router, discovery_router) = if tools.discovery {
            let mut visible = Self::discovery_router();
            discovery::annotate(&mut visible, tools.read_only);
            (visible, Some(assembled.router))
        } else {
            (assembled.router, None)
        };
        tracing::info!(
            served = tool_router.map.len(),
            discoverable = domain_tool_count,
            withheld = assembled.withheld,
            read_only = tools.read_only,
            discovery = tools.discovery,
            groups = assembled
                .groups
                .iter()
                .map(|g| g.as_str())
                .collect::<Vec<_>>()
                .join(","),
            "assembled tool router"
        );

        Self {
            client: Arc::new(AscClient::new(config)),
            market: Arc::new(market::MarketClient::default()),
            tool_router,
            discovery_router,
            policy: None,
            pending: Arc::new(PendingStore::from_env()),
            index: Arc::new(Self::tool_index().into_iter().map(|(n, g, t)| (n, (g, t))).collect()),
        }
    }

    /// OMB fork: serve one profile. Adds `list_access` and `confirm_action`;
    /// discovery mode is switched off because it would route around the
    /// per-call check.
    pub fn with_policy(mut self, policy: Policy, pending: PendingStore) -> Self {
        if let Some(discovery) = self.discovery_router.take() {
            tracing::warn!("ASC_TOOL_DISCOVERY is ignored when ASC_PROFILE is set");
            self.tool_router = discovery;
        }
        let mut access = Self::access_router();
        for route in access.map.values_mut() {
            route.attr.annotations = Some(if route.attr.name == "list_access" {
                ToolAnnotations::new()
                    .open_world(false)
                    .read_only(true)
                    .destructive(false)
                    .idempotent(true)
            } else {
                // Runs whatever was held: never safe to auto-approve.
                ToolAnnotations::new()
                    .open_world(true)
                    .read_only(false)
                    .destructive(true)
                    .idempotent(false)
            });
        }
        self.tool_router = self.tool_router + access;
        tracing::info!(
            profile = %policy.profile_name(),
            file = %policy.path().display(),
            "serving one profile; tools it does not open are closed"
        );
        self.policy = Some(Arc::new(policy));
        self.pending = Arc::new(pending);
        self
    }

    /// Every domain tool with its group and tier, sorted by name. Independent
    /// of `ASC_TOOLS` and `ASC_READ_ONLY`.
    pub fn tool_index() -> Vec<(String, Group, Tier)> {
        let mut out = Vec::new();
        for (group, router) in Self::domain_routers() {
            for name in router.map.keys() {
                let tier = policy::tier_of(name, group, catalog::classify(name));
                out.push((name.to_string(), group, tier));
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out
    }

    /// Access to one tool call under `profile`, including the API operation
    /// for the generic tools.
    pub(crate) fn access_for(
        &self,
        profile: &Profile,
        name: &str,
        arguments: Option<&serde_json::Map<String, Value>>,
        now: u64,
    ) -> Access {
        let arg = |key: &str| {
            arguments
                .and_then(|a| a.get(key))
                .and_then(Value::as_str)
                .map(str::to_string)
        };
        let simple = |open: bool| {
            if open {
                Access::Open { confirm: false, until: None }
            } else {
                Access::Closed
            }
        };
        match name {
            "list_access" | "confirm_action" => Access::Open { confirm: false, until: None },
            "api_search" | "api_describe" => simple(profile.has_api(now)),
            "api_execute" => match (arg("method"), arg("path")) {
                (Some(method), Some(path)) => profile.api_access(&method, &path, now),
                // Listing: shown when any operation is open.
                _ if arguments.is_none() => simple(profile.has_api(now)),
                _ => Access::Closed,
            },
            "api_list" => match arg("cursor").or_else(|| arg("path")) {
                Some(path) => profile.api_access("GET", &path, now),
                None if arguments.is_none() => simple(profile.has_api_method("GET", now)),
                None => Access::Closed,
            },
            _ => match self.index.get(name) {
                Some((group, tier)) => profile.tool_access(name, *group, *tier, now),
                None => Access::Closed,
            },
        }
    }

    /// The tools a caller sees: all of them without a profile, else the open ones.
    fn visible_tools(&self) -> Vec<Tool> {
        let Some(policy) = &self.policy else {
            return self.tools();
        };
        let profile = policy.current();
        let now = policy::now_unix();
        self.tools()
            .into_iter()
            .filter(|t| self.access_for(&profile, &t.name, None, now).is_open())
            .collect()
    }

    /// Check a call against the profile. `Ok(None)` runs it, `Ok(Some(_))` is
    /// the reply for a held call.
    fn gate(&self, request: &CallToolRequestParams) -> Result<Option<CallToolResult>, McpError> {
        let Some(policy) = &self.policy else {
            return Ok(None);
        };
        let name: &str = &request.name;
        let profile = policy.current();
        let now = policy::now_unix();
        let arguments = request.arguments.as_ref();
        match self.access_for(&profile, name, arguments, now) {
            Access::Closed => {
                tracing::info!(profile = %policy.profile_name(), tool = %name, "closed");
                let what = match name {
                    "api_execute" | "api_list" => format!(
                        "this API operation is not open for this connection ({name})"
                    ),
                    _ => format!("{name} is not open for this connection"),
                };
                Err(McpError::invalid_params(
                    format!("{what}. Only the owner can open it; say what you need it for."),
                    None,
                ))
            }
            Access::Open { confirm: false, .. } => {
                tracing::info!(profile = %policy.profile_name(), tool = %name, "run");
                Ok(None)
            }
            Access::Open { confirm: true, .. } => {
                // Refuse a call that cannot run before handing out a token.
                if let Some(tool) = self.tool_router.get(name) {
                    let missing: Vec<&str> = tool
                        .input_schema
                        .get("required")
                        .and_then(Value::as_array)
                        .into_iter()
                        .flatten()
                        .filter_map(Value::as_str)
                        .filter(|k| !arguments.is_some_and(|a| a.contains_key(*k)))
                        .collect();
                    if !missing.is_empty() {
                        return Err(McpError::invalid_params(
                            format!("{name}: missing required argument(s) {}; nothing held", missing.join(", ")),
                            None,
                        ));
                    }
                }
                let call = PendingCall {
                    profile: policy.profile_name().to_string(),
                    tool: name.to_string(),
                    arguments: arguments.cloned(),
                    created: now,
                    expires: now + policy::PENDING_TTL_SECS,
                };
                let token = self
                    .pending
                    .put(&call)
                    .map_err(|e| McpError::internal_error(e, None))?;
                tracing::info!(profile = %policy.profile_name(), tool = %name, "held");
                let mut preview = serde_json::to_string_pretty(&call.arguments)
                    .unwrap_or_else(|_| "{}".into());
                if preview.len() > 4000 {
                    let mut cut = 4000;
                    while !preview.is_char_boundary(cut) {
                        cut -= 1;
                    }
                    preview.truncate(cut);
                    preview.push_str("\n…");
                }
                Ok(Some(CallToolResult::success(vec![Content::text(format!(
                    "NOT RUN YET. {name} changes what customers see or what they pay, or cannot \
                     be undone, so it is held. Check the call below. To run it exactly as shown, \
                     call confirm_action with token {token} within {} minutes. To change it, \
                     call {name} again.\n\nArguments:\n{preview}",
                    policy::PENDING_TTL_SECS / 60
                ))])))
            }
        }
    }

    /// Every domain router, tagged with the group it belongs to.
    fn domain_routers() -> Vec<(Group, ToolRouter<Self>)> {
        vec![
            (Group::Generic, Self::generic_router()),
            (Group::Apps, Self::apps_router()),
            (Group::Iap, Self::iap_router()),
            (Group::Subscriptions, Self::subscriptions_router()),
            (Group::Versions, Self::versions_router()),
            (Group::Pricing, Self::pricing_router()),
            (Group::Availability, Self::availability_router()),
            (Group::Submission, Self::submission_router()),
            (Group::TestFlight, Self::testflight_router()),
            (Group::Provisioning, Self::provisioning_router()),
            (
                Group::Assets,
                Self::assets_router() + Self::asset_library_router(),
            ),
            (Group::Offers, Self::offers_router()),
            (Group::OfferCodes, Self::offer_codes_router()),
            (Group::Promotions, Self::promotions_router()),
            (Group::Reviews, Self::reviews_router()),
            (Group::Users, Self::users_router()),
            (Group::Events, Self::events_router()),
            (Group::XcodeCloud, Self::xcode_cloud_router()),
            (
                Group::Analytics,
                Self::analytics_router() + Self::reports_router(),
            ),
            (
                Group::CustomProductPages,
                Self::custom_product_pages_router(),
            ),
            (Group::Market, Self::market_router()),
            (Group::Webhooks, Self::webhooks_router()),
        ]
    }

    /// The tools this server serves, sorted by name.
    pub fn tools(&self) -> Vec<Tool> {
        let mut tools: Vec<Tool> = self
            .tool_router
            .map
            .values()
            .map(|route| route.attr.clone())
            .collect();
        tools.sort_by(|a, b| a.name.cmp(&b.name));
        tools
    }

    /// Render a JSON value as a text tool result, compacted and size-capped per
    /// [`crate::json`] so one verbose page can't swamp the agent's context.
    pub(crate) fn ok_json(&self, value: Value) -> Result<CallToolResult, McpError> {
        let text = crate::json::render(value, &self.client.config.output);
        Ok(CallToolResult::success(vec![Content::text(text)]))
    }

    /// Refuse an operation that would modify the account on a read-only server.
    pub(crate) fn ensure_writable(&self, what: &str) -> Result<(), McpError> {
        if self.client.config.tools.read_only {
            return Err(AscError::InvalidRequest(format!(
                "this server is running in read-only mode (ASC_READ_ONLY), so {what} is not \
                 permitted; unset ASC_READ_ONLY to allow writes"
            ))
            .into_mcp_error());
        }
        Ok(())
    }

    /// Map a client error into an MCP tool error.
    pub(crate) fn map_err(err: AscError) -> McpError {
        err.into_mcp_error()
    }
}

/// Flatten a JSON object of query parameters into pre-stringified pairs.
///
/// Array values are comma-joined (App Store Connect's convention for repeated
/// filters and `include`/`fields` lists); scalars are stringified.
pub(crate) fn flatten_query(map: &serde_json::Map<String, Value>) -> Vec<(String, String)> {
    map.iter()
        .map(|(k, v)| (k.clone(), value_to_query_string(v)))
        .collect()
}

fn value_to_query_string(value: &Value) -> String {
    match value {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        Value::Array(arr) => arr
            .iter()
            .map(value_to_query_string)
            .collect::<Vec<_>>()
            .join(","),
        other => other.to_string(),
    }
}

/// Push `(key, value)` onto a query vector when the option is `Some`.
pub(crate) fn push_opt(query: &mut Vec<(String, String)>, key: &str, value: Option<impl ToString>) {
    if let Some(v) = value {
        query.push((key.to_string(), v.to_string()));
    }
}

/// Insert a string attribute into a JSON object only when the option is `Some`.
pub(crate) fn set_opt_str(obj: &mut serde_json::Value, key: &str, value: &Option<String>) {
    if let Some(v) = value {
        obj[key] = serde_json::json!(v);
    }
}

/// Coerce a tool-argument value that may have been sent as a *stringified* JSON
/// document back into the real parsed value.
///
/// Several MCP clients serialize object/array arguments as JSON strings (e.g.
/// they send `body` as `"{\"data\":{...}}"` instead of `{"data":{...}}`). Such a
/// string would otherwise be re-encoded as a quoted JSON *string* in the outbound
/// request and rejected by the API as "not a valid request document object".
/// Strings that look like a JSON object/array and parse successfully are replaced
/// with their parsed value; every other value — including ordinary non-JSON
/// strings and already-structured objects — passes through unchanged, so
/// faithfully-encoded arguments are never altered.
pub(crate) fn coerce_json(value: Value) -> Value {
    if let Value::String(s) = &value {
        let trimmed = s.trim_start();
        if trimmed.starts_with('{') || trimmed.starts_with('[') {
            if let Ok(parsed) = serde_json::from_str::<Value>(s) {
                return parsed;
            }
        }
    }
    value
}

/// `#[serde(deserialize_with)]` adapter applying [`coerce_json`] to a required
/// `Value` field.
pub(crate) fn de_coerce_json<'de, D>(deserializer: D) -> Result<Value, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(coerce_json(Value::deserialize(deserializer)?))
}

/// `#[serde(deserialize_with)]` adapter applying [`coerce_json`] to an optional
/// `Value` field. Pair with `#[serde(default)]`.
pub(crate) fn de_coerce_json_opt<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Ok(Option::<Value>::deserialize(deserializer)?.map(coerce_json))
}

/// `#[serde(deserialize_with)]` adapter for an optional query/filter map that also
/// accepts a stringified JSON object. Pair with `#[serde(default)]`.
pub(crate) fn de_coerce_map_opt<'de, D>(
    deserializer: D,
) -> Result<Option<serde_json::Map<String, Value>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    match Option::<Value>::deserialize(deserializer)?.map(coerce_json) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Object(map)) => Ok(Some(map)),
        Some(other) => Err(serde::de::Error::custom(format!(
            "expected an object (or a JSON-object string) but got: {other}"
        ))),
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for AppStoreServer {
    // OMB fork: these three replace the macro's versions so a profile can
    // hide and gate tools. Without a profile they behave as upstream.
    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, McpError> {
        Ok(ListToolsResult {
            tools: self.visible_tools(),
            meta: None,
            next_cursor: None,
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        if let Some(held) = self.gate(&request)? {
            return Ok(held);
        }
        let tcc = rmcp::handler::server::tool::ToolCallContext::new(self, request, context);
        self.tool_router.call(tcc).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.visible_tools().into_iter().find(|t| t.name == name)
    }

    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::from_build_env())
            .with_instructions(if self.discovery_router.is_some() {
                DISCOVERY_INSTRUCTIONS.to_string()
            } else {
                INSTRUCTIONS.to_string()
            })
    }
}

#[cfg(test)]
mod tests {
    use super::{coerce_json, generic, versions};
    use serde_json::{json, Value};

    #[test]
    fn coerce_json_parses_stringified_object() {
        assert_eq!(
            coerce_json(Value::String(r#"{"whatsNew":"hi"}"#.to_string())),
            json!({ "whatsNew": "hi" })
        );
    }

    #[test]
    fn coerce_json_parses_stringified_array() {
        assert_eq!(
            coerce_json(Value::String("[1, 2, 3]".to_string())),
            json!([1, 2, 3])
        );
    }

    #[test]
    fn coerce_json_passes_through_structured_value() {
        let v = json!({ "data": { "type": "apps" } });
        assert_eq!(coerce_json(v.clone()), v);
    }

    #[test]
    fn coerce_json_leaves_plain_string_untouched() {
        let v = Value::String("Bug fixes and a fresh new look.".to_string());
        assert_eq!(coerce_json(v.clone()), v);
    }

    #[test]
    fn coerce_json_leaves_unparseable_jsonish_string_untouched() {
        let v = Value::String("{not valid json".to_string());
        assert_eq!(coerce_json(v.clone()), v);
    }

    #[test]
    fn request_args_coerces_stringified_body() {
        // Reproduces the real bug: a client that sends `body` as a JSON *string*.
        let args: generic::RequestArgs = serde_json::from_value(json!({
            "method": "PATCH",
            "path": "/v1/appStoreVersionLocalizations/x",
            "body": "{\"data\":{\"type\":\"appStoreVersionLocalizations\",\"id\":\"x\",\"attributes\":{\"whatsNew\":\"hi\"}}}"
        }))
        .unwrap();
        let body = args.body.expect("body present");
        assert!(
            body.is_object(),
            "stringified body must coerce to an object, got: {body}"
        );
        assert_eq!(body["data"]["attributes"]["whatsNew"], "hi");
    }

    #[test]
    fn request_args_keeps_real_object_body() {
        let args: generic::RequestArgs = serde_json::from_value(json!({
            "method": "POST",
            "path": "/v1/apps",
            "body": { "data": { "type": "apps", "attributes": { "name": "x" } } }
        }))
        .unwrap();
        assert_eq!(args.body.unwrap()["data"]["type"], "apps");
    }

    #[test]
    fn update_localization_attributes_coerces_stringified() {
        let args: versions::UpdateVersionLocalizationArgs = serde_json::from_value(json!({
            "localization_id": "loc-1",
            "attributes": "{\"whatsNew\":\"hi\"}"
        }))
        .unwrap();
        assert_eq!(args.attributes, json!({ "whatsNew": "hi" }));
    }
}
