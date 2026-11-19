//! Profile tools (OMB fork): what this caller may use, and running a held
//! `release`/`danger` call. Only served when a profile is set (`ASC_PROFILE`);
//! see [`crate::policy`].

use rmcp::{
    handler::server::{tool::ToolCallContext, wrapper::Parameters},
    model::*,
    schemars, tool, tool_router, ErrorData as McpError, RoleServer,
};
use serde::Deserialize;
use serde_json::{json, Value};

use super::AppStoreServer;
use crate::policy::{format_utc, now_unix, Access};

#[derive(Debug, Deserialize, schemars::JsonSchema)]
pub struct ConfirmActionArgs {
    /// The token returned when the action was requested.
    token: String,
}

#[tool_router(router = access_router, vis = "pub(crate)")]
impl AppStoreServer {
    /// What this profile may use right now.
    #[tool(
        description = "List what this App Store Connect connection may use right now: open tools \
with their tier and expiry, open API operations, and how many tools are closed per group. A \
closed tool can only be opened by the owner; name it when asking."
    )]
    async fn list_access(&self) -> Result<CallToolResult, McpError> {
        let Some(policy) = &self.policy else {
            return Err(McpError::invalid_request("no profile is set", None));
        };
        let profile = policy.current();
        let now = now_unix();

        let mut open = Vec::new();
        let mut closed: std::collections::BTreeMap<&str, usize> = Default::default();
        for (name, (group, tier)) in self.index.iter() {
            // The api_* tools are reported under openApiOperations.
            if self.tool_router.get(name).is_none() || *group == crate::server::catalog::Group::Generic {
                continue;
            }
            match profile.tool_access(name, *group, *tier, now) {
                Access::Open { confirm, until } => open.push(json!({
                    "tool": name,
                    "tier": tier.as_str(),
                    "needsConfirm": confirm,
                    "until": until.map(format_utc),
                })),
                Access::Closed => *closed.entry(group.as_str()).or_default() += 1,
            }
        }
        let api: Vec<Value> = profile
            .api
            .iter()
            .filter(|g| match &g.until {
                None => true,
                Some(u) => crate::policy::parse_utc(u).is_some_and(|u| u > now),
            })
            .map(|g| {
                json!({
                    "method": g.method.to_ascii_uppercase(),
                    "path": g.path,
                    "until": g.until,
                    "needsConfirm": profile.confirm && !g.method.eq_ignore_ascii_case("GET"),
                })
            })
            .collect();

        self.ok_json(json!({
            "profile": policy.profile_name(),
            "openTools": open,
            "openApiOperations": api,
            "closedToolsByGroup": closed,
            "note": "needsConfirm tools return a token instead of acting; pass it to confirm_action.",
        }))
    }

    /// Run a held call.
    #[tool(
        description = "Run a release- or danger-tier action that a previous tool call held back. \
Pass the token from that call's reply. The exact stored call runs once; the token expires after \
10 minutes and access is checked again first. Confirm only what you meant to do."
    )]
    async fn confirm_action(
        &self,
        Parameters(args): Parameters<ConfirmActionArgs>,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<CallToolResult, McpError> {
        let Some(policy) = &self.policy else {
            return Err(McpError::invalid_request("no profile is set", None));
        };
        let now = now_unix();
        let call = self
            .pending
            .take(&args.token, now)
            .map_err(|e| McpError::invalid_params(e, None))?;
        if call.profile != policy.profile_name() {
            return Err(McpError::invalid_params("unknown or expired token", None));
        }
        let profile = policy.current();
        if let Access::Closed = self.access_for(&profile, &call.tool, call.arguments.as_ref(), now) {
            tracing::warn!(profile = %policy.profile_name(), tool = %call.tool, "confirm refused: closed since held");
            return Err(McpError::invalid_params(
                format!("{} was closed since the call was held; nothing ran", call.tool),
                None,
            ));
        }
        tracing::info!(profile = %policy.profile_name(), tool = %call.tool, "confirmed");
        let mut request = CallToolRequestParams::new(call.tool);
        if let Some(arguments) = call.arguments {
            request = request.with_arguments(arguments);
        }
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use rmcp::model::CallToolRequestParams;
    use serde_json::{json, Value};

    use super::AppStoreServer;
    use crate::config::Config;
    use crate::policy::{PendingStore, Policy};

    static N: AtomicUsize = AtomicUsize::new(0);

    fn server_with(profile: Value) -> (AppStoreServer, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "asc-mcp-srv-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("profiles.json");
        std::fs::write(&file, json!({ "profiles": { "p": profile } }).to_string()).unwrap();
        let server = AppStoreServer::new(Config::from_parts(None, None, None))
            .with_policy(Policy::new(file, "p".into()), PendingStore::new(dir.join("pending")));
        (server, dir)
    }

    fn names(s: &AppStoreServer) -> Vec<String> {
        s.visible_tools().into_iter().map(|t| t.name.to_string()).collect()
    }

    fn call(name: &'static str, args: Value) -> CallToolRequestParams {
        CallToolRequestParams::new(name).with_arguments(args.as_object().unwrap().clone())
    }

    #[test]
    fn everything_is_closed_by_default() {
        let (s, _) = server_with(json!({}));
        assert_eq!(names(&s), vec!["confirm_action", "list_access"]);
        assert!(s.gate(&CallToolRequestParams::new("list_apps")).is_err());
    }

    #[test]
    fn a_missing_profile_closes_everything() {
        let (s, dir) = server_with(json!({ "open": [{ "select": "tier:read" }] }));
        let other = AppStoreServer::new(Config::from_parts(None, None, None)).with_policy(
            Policy::new(dir.join("profiles.json"), "nobody".into()),
            PendingStore::new(dir.join("pending")),
        );
        assert!(names(&s).len() > 2);
        assert_eq!(names(&other), vec!["confirm_action", "list_access"]);
    }

    #[test]
    fn the_read_tier_shows_reads_only() {
        let (s, _) = server_with(json!({ "open": [{ "select": "tier:read" }] }));
        let n = names(&s);
        assert!(n.contains(&"list_apps".to_string()));
        assert!(n.contains(&"download_sales_report".to_string()));
        for t in &n {
            assert!(
                !(t.starts_with("create_") || t.starts_with("delete_") || t.starts_with("update_")),
                "{t}"
            );
        }
        assert!(!n.contains(&"api_execute".to_string()));
        assert!(!n.contains(&"search_store_apps".to_string()), "market is tier public");
    }

    #[test]
    fn release_calls_are_held_and_confirm_is_single_use() {
        let (s, dir) = server_with(json!({ "open": [{ "select": "start_phased_release" }] }));
        let bad = call("start_phased_release", json!({ "app_store_version_id": "1" }));
        assert!(s.gate(&bad).is_err(), "missing required args are refused, not held");
        assert!(!dir.join("pending").exists() || std::fs::read_dir(dir.join("pending")).unwrap().count() == 0);
        let req = call("start_phased_release", json!({ "version_id": "1" }));
        let held = s.gate(&req).unwrap().expect("held, not run");
        let text = held.content[0].as_text().unwrap().text.clone();
        assert!(text.starts_with("NOT RUN YET"), "{text}");
        let token = text
            .split_whitespace()
            .find(|w| w.len() == 32 && w.bytes().all(|b| b.is_ascii_hexdigit()))
            .expect("token in reply");
        assert_eq!(std::fs::read_dir(dir.join("pending")).unwrap().count(), 1);
        let call = s.pending.take(token, crate::policy::now_unix()).unwrap();
        assert_eq!(call.tool, "start_phased_release");
        assert_eq!(call.arguments.unwrap()["version_id"], "1");
        assert!(s.pending.take(token, crate::policy::now_unix()).is_err());
    }

    #[test]
    fn write_tier_runs_directly() {
        let (s, _) = server_with(json!({ "open": [{ "select": "respond_to_review" }] }));
        let req = call("respond_to_review", json!({ "review_id": "1", "response_body": "Thanks" }));
        assert!(s.gate(&req).unwrap().is_none());
    }

    #[test]
    fn api_calls_are_checked_per_operation() {
        let (s, _) = server_with(json!({ "api": [
            { "method": "GET", "path": "/v1/apps/*/customerReviews" },
            { "method": "PATCH", "path": "/v1/appStoreVersionLocalizations/*" }
        ] }));
        let n = names(&s);
        for t in ["api_execute", "api_list", "api_search", "api_describe"] {
            assert!(n.contains(&t.to_string()), "{t}");
        }
        let ok = call("api_execute", json!({ "method": "GET", "path": "/v1/apps/1/customerReviews" }));
        assert!(s.gate(&ok).unwrap().is_none());
        let other = call("api_execute", json!({ "method": "GET", "path": "/v1/users" }));
        assert!(s.gate(&other).is_err());
        let write = call(
            "api_execute",
            json!({ "method": "PATCH", "path": "/v1/appStoreVersionLocalizations/9", "body": {} }),
        );
        assert!(s.gate(&write).unwrap().is_some(), "non-GET is held");
        let page = call(
            "api_list",
            json!({ "cursor": "https://api.appstoreconnect.apple.com/v1/apps/1/customerReviews?cursor=x" }),
        );
        assert!(s.gate(&page).unwrap().is_none());
        let sneaky = call("api_list", json!({ "path": "/v1/apps/1/customerReviews/../../users" }));
        assert!(s.gate(&sneaky).is_err());
    }

    #[test]
    fn file_changes_apply_without_restart() {
        let (s, dir) = server_with(json!({}));
        assert!(!names(&s).contains(&"list_apps".to_string()));
        std::fs::write(
            dir.join("profiles.json"),
            json!({ "profiles": { "p": { "open": [{ "select": "list_apps" }] } } }).to_string(),
        )
        .unwrap();
        assert!(names(&s).contains(&"list_apps".to_string()));
    }
}
