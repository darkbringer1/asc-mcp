# asc-mcp, OMB fork

Fork of [n0ah37/asc-mcp](https://github.com/n0ah37/asc-mcp) (itself a fork of
[forgeopslabs/appstore-mcp](https://github.com/forgeopslabs/appstore-mcp)),
MIT. All 145 upstream tools stay compiled in. This fork adds **profiles**: one
process serves one caller, and every tool that profile doesn't open is closed
(not listed, not callable).

Remotes: `origin` = darkbringer1/asc-mcp (GitHub Actions off),
`upstream` = n0ah37/asc-mcp, `forgeops` = forgeopslabs/appstore-mcp.
Our changes: `src/policy.rs`, `src/cli.rs`, `src/server/access.rs`, the
handler overrides in `src/server/mod.rs`, `src/main.rs`, and this folder.

## Profiles

`ASC_PROFILE=<name>` turns profiles on; the file is `ASC_PROFILES` (default
`/etc/asc-mcp/profiles.json`). The file is re-read when it changes, so opening
or closing needs no restart. A missing file, a parse error or an unknown
profile closes everything. Without `ASC_PROFILE` the server behaves as
upstream (all tools), so OMB entries must always set it.

Tiers (`asc-mcp tools` prints every tool with its tier and group):

| Tier | What | Opened by |
|---|---|---|
| `public` | market group: public App Store data, no key | name, group, `tier:public` |
| `read` | reads the account | name, group, `tier:read` |
| `write` | stays in ASC until shipped: metadata, screenshots, review replies, TestFlight notes | name, group, `tier:write` |
| `release` | reaches customers or money: review submission, phased release, pricing, availability, IAP, subscriptions, offers | name, group, `tier:release`; held for confirm |
| `danger` | deletes, users, signing, webhooks, unclassified | exact name + `"dangerous": true` only; held for confirm |

Held calls: a `release`/`danger` call (and any non-GET `api_execute`) does not
run. The reply shows the arguments and a token; `confirm_action(token)` runs
that exact stored call once, within 10 minutes, after checking access again.
`"confirm": false` on a profile turns holding off. Held calls live in
`ASC_STATE_DIR/pending` (default `~/.local/state/asc-mcp/pending`), 600.

Generic API (`api_execute`, `api_list`, `api_search`, `api_describe`): opened
per operation by `api` entries, method + path pattern (`*` one segment, final
`**` the rest). Paths with `..`, `%`, `//` or `\` are refused. `DELETE` needs
`"dangerous": true`.

Every profile also gets `list_access` (what is open, what is closed per group)
and `confirm_action`.

```json
{
  "profiles": {
    "scout": {
      "note": "Amelia: competitors and our reviews",
      "open": [
        { "select": "tier:public" },
        { "select": "list_apps" },
        { "select": "list_customer_reviews" }
      ]
    },
    "steve": {
      "open": [
        { "select": "tier:public" },
        { "select": "tier:read" },
        { "select": "respond_to_review", "until": "2026-12-01T18:00:00Z" }
      ],
      "deny": ["group:users", "group:provisioning", "download_finance_report"],
      "api": [{ "method": "GET", "path": "/v1/apps/*/perfPowerMetrics" }]
    }
  }
}
```

## Owner commands

Run on the server as root (the file is root-owned, bots only read it):

```sh
asc-mcp tools --profile steve                 # every tool, open or closed for steve
asc-mcp profile show steve
asc-mcp profile open steve respond_to_review --for 2h
asc-mcp profile open steve group:testflight
asc-mcp profile open steve delete_review_response --dangerous --for 30m
asc-mcp profile close steve respond_to_review
asc-mcp profile api-open steve GET '/v1/apps/*/customerReviews'
asc-mcp profile api-close steve GET '/v1/apps/*/customerReviews'
asc-mcp profile check
```

## Limits

- The API key's role is the only hard ceiling. Bots run as the OMB user,
  which has passwordless sudo, so profiles stop mistakes, not a determined
  bot. Use a Sales + Customer Support key for read and reply profiles; an
  App Manager key only where a profile really releases.
- `ASC_TOOLS` and `ASC_READ_ONLY` still apply on top of a profile.
  `ASC_TOOL_DISCOVERY` is ignored with a profile (it would route around the
  per-call check).
- OMB caches each server's tool list (`~/.openmausbot/mcp-app-catalog.json`);
  its MCP Test refreshes it, so press Test after opening or closing a tool.

## Build

No Rust on the Mac host (owner rule). Build VM: `omb/lima-build.yaml`
(`limactl start --name=asc-build omb/lima-build.yaml`), Ubuntu 24.04 ARM64
like server `omb`. `cargo test` and `cargo build --release` there.

## On server omb

Installed by `OpenMausBot/install-asc-mcp.sh` (workbench repo): binary
`/usr/local/bin/asc-mcp`, system user `ascmcp`, `/etc/asc-mcp/profiles.json`
and `/etc/asc-mcp/env` (root:ascmcp 640), key and held calls in
`/var/lib/asc-mcp` (ascmcp 700). OMB entries `asc-<profile>` run
`/usr/local/bin/asc-mcp-omb <profile>`, which starts the server as `ascmcp`.
