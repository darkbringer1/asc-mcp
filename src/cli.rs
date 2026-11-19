//! Owner commands (OMB fork). Run by the owner on the server, never by a bot:
//!
//! ```text
//! asc-mcp tools [--profile NAME]                 every tool: tier, group, open?
//! asc-mcp profile list
//! asc-mcp profile show NAME
//! asc-mcp profile open NAME SELECTOR [--for 2h | --until TS] [--dangerous]
//! asc-mcp profile close NAME SELECTOR
//! asc-mcp profile api-open NAME METHOD PATH [--for 2h | --until TS] [--dangerous]
//! asc-mcp profile api-close NAME METHOD PATH
//! asc-mcp profile check
//! ```
//!
//! SELECTOR is a tool name, `group:<group>` or `tier:<tier>`. The file is
//! `ASC_PROFILES` (default `/etc/asc-mcp/profiles.json`) or `--file PATH`.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};

use crate::policy::{
    self, format_utc, load_file, now_unix, parse_duration, parse_utc, Access, ApiGrant, Grant,
    Profile, ProfileFile, Selector, Tier,
};
use crate::server::catalog::Group;
use crate::server::AppStoreServer;

const API_TOOLS: &[&str] = &["api_execute", "api_list", "api_search", "api_describe"];
const METHODS: &[&str] = &["GET", "POST", "PATCH", "PUT", "DELETE"];

/// Run a CLI command if `args` (without the program name) names one.
/// Returns `None` when the process should serve MCP instead.
pub fn run(args: &[String]) -> Option<Result<()>> {
    let first = args.first()?.as_str();
    Some(match first {
        "tools" => cmd_tools(&args[1..]),
        "profile" => cmd_profile(&args[1..]),
        "-h" | "--help" | "help" => {
            print!("{}", USAGE);
            Ok(())
        }
        other => Err(anyhow!("unknown command '{other}'\n\n{USAGE}")),
    })
}

const USAGE: &str = "\
asc-mcp                       serve MCP on stdio (ASC_PROFILE picks the profile)
asc-mcp tools [--profile NAME]
asc-mcp profile list | show NAME | check
asc-mcp profile open NAME SELECTOR [--for 2h|--until 2026-12-01T18:00:00Z] [--dangerous]
asc-mcp profile close NAME SELECTOR
asc-mcp profile api-open NAME METHOD PATH [--for ...|--until ...] [--dangerous]
asc-mcp profile api-close NAME METHOD PATH
SELECTOR: tool name, group:<group> or tier:<public|read|write|release|danger>
Options: --file PATH (else ASC_PROFILES, else /etc/asc-mcp/profiles.json)
";

/// Parsed flags shared by the commands.
#[derive(Default)]
struct Flags {
    positional: Vec<String>,
    file: Option<PathBuf>,
    profile: Option<String>,
    for_: Option<String>,
    until: Option<String>,
    dangerous: bool,
}

fn parse_flags(args: &[String]) -> Result<Flags> {
    let mut f = Flags::default();
    let mut it = args.iter();
    while let Some(a) = it.next() {
        let mut value = |name: &str| {
            it.next()
                .cloned()
                .ok_or_else(|| anyhow!("{name} needs a value"))
        };
        match a.as_str() {
            "--file" => f.file = Some(PathBuf::from(value("--file")?)),
            "--profile" => f.profile = Some(value("--profile")?),
            "--for" => f.for_ = Some(value("--for")?),
            "--until" => f.until = Some(value("--until")?),
            "--dangerous" => f.dangerous = true,
            s if s.starts_with("--") => bail!("unknown option {s}"),
            _ => f.positional.push(a.clone()),
        }
    }
    Ok(f)
}

fn profiles_path(flags: &Flags) -> PathBuf {
    flags.file.clone().unwrap_or_else(|| {
        std::env::var("ASC_PROFILES")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(policy::DEFAULT_PROFILES_PATH))
    })
}

fn read_or_empty(path: &Path) -> Result<ProfileFile> {
    if !path.exists() {
        return Ok(ProfileFile::default());
    }
    load_file(path).map_err(|e| anyhow!(e))
}

/// Write the file next to itself and rename, keeping the old file's mode.
fn write_atomic(path: &Path, file: &ProfileFile) -> Result<()> {
    let text = serde_json::to_string_pretty(file)? + "\n";
    let dir = path.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
    let tmp = dir.join(format!(
        ".{}.tmp{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("profiles.json"),
        std::process::id()
    ));
    std::fs::write(&tmp, text).with_context(|| format!("cannot write {}", tmp.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        // Keep mode, owner and group: the server reads the file through its
        // group (root:ascmcp 640 on omb), and a plain rename would drop that.
        match std::fs::metadata(path) {
            Ok(old) => {
                std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(old.mode()))?;
                std::os::unix::fs::chown(&tmp, Some(old.uid()), Some(old.gid()))
                    .with_context(|| format!("cannot keep owner of {}", path.display()))?;
            }
            Err(_) => std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o644))?,
        }
    }
    std::fs::rename(&tmp, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

/// `--for` / `--until` into an `until` string.
fn until_from(flags: &Flags) -> Result<Option<String>> {
    match (&flags.for_, &flags.until) {
        (Some(_), Some(_)) => bail!("use --for or --until, not both"),
        (Some(d), None) => {
            let secs = parse_duration(d).ok_or_else(|| anyhow!("--for takes e.g. 90m, 2h, 7d"))?;
            Ok(Some(format_utc(now_unix() + secs)))
        }
        (None, Some(u)) => {
            let at = parse_utc(u).ok_or_else(|| anyhow!("--until takes UTC like 2026-12-01T18:00:00Z"))?;
            if at <= now_unix() {
                bail!("--until is in the past");
            }
            Ok(Some(format_utc(at)))
        }
        (None, None) => Ok(None),
    }
}

fn describe(access: Access) -> String {
    match access {
        Access::Closed => "closed".into(),
        Access::Open { confirm, until } => {
            let mut s = "open".to_string();
            if confirm {
                s.push_str(", needs confirm");
            }
            if let Some(u) = until {
                s.push_str(&format!(", until {}", format_utc(u)));
            }
            s
        }
    }
}

fn cmd_tools(args: &[String]) -> Result<()> {
    let flags = parse_flags(args)?;
    let profile = match &flags.profile {
        Some(name) => Some(
            read_or_empty(&profiles_path(&flags))?
                .profiles
                .get(name)
                .cloned()
                .ok_or_else(|| anyhow!("no profile '{name}'"))?,
        ),
        None => None,
    };
    let now = now_unix();
    for (name, group, tier) in AppStoreServer::tool_index() {
        let state = profile
            .as_ref()
            .map(|p| format!("  {}", describe(p.tool_access(&name, group, tier, now))))
            .unwrap_or_default();
        println!("{:<8} {:<22} {}{}", tier.as_str(), group.as_str(), name, state);
    }
    for name in API_TOOLS {
        println!("{:<8} {:<22} {}  (opened per operation with api-open)", "api", "generic", name);
    }
    Ok(())
}

fn cmd_profile(args: &[String]) -> Result<()> {
    let flags = parse_flags(args)?;
    let path = profiles_path(&flags);
    let pos = &flags.positional;
    let sub = pos.first().map(String::as_str).unwrap_or("list");
    let need = |n: usize, usage: &str| -> Result<()> {
        if pos.len() != n {
            bail!("usage: asc-mcp profile {usage}");
        }
        Ok(())
    };

    match sub {
        "list" => {
            let file = read_or_empty(&path)?;
            if file.profiles.is_empty() {
                println!("no profiles in {}", path.display());
            }
            for (name, p) in &file.profiles {
                println!(
                    "{name}: {} open, {} deny, {} api{}",
                    p.open.len(),
                    p.deny.len(),
                    p.api.len(),
                    p.note.as_deref().map(|n| format!("  ({n})")).unwrap_or_default()
                );
            }
            Ok(())
        }
        "check" => {
            let file = load_file(&path).map_err(|e| anyhow!(e))?;
            let problems = check(&file);
            for p in &problems {
                println!("{p}");
            }
            if problems.is_empty() {
                println!("{}: {} profiles, ok", path.display(), file.profiles.len());
                Ok(())
            } else {
                bail!("{} problem(s)", problems.len())
            }
        }
        "show" => {
            need(2, "show NAME")?;
            let file = read_or_empty(&path)?;
            let p = file
                .profiles
                .get(&pos[1])
                .ok_or_else(|| anyhow!("no profile '{}'", pos[1]))?;
            show(&pos[1], p);
            Ok(())
        }
        "open" => {
            need(3, "open NAME SELECTOR [--for 2h|--until TS] [--dangerous]")?;
            let (name, raw) = (&pos[1], pos[2].trim());
            let selector = Selector::parse(raw).ok_or_else(|| anyhow!("bad selector '{raw}'"))?;
            let index = AppStoreServer::tool_index();
            match &selector {
                Selector::Tool(t) if API_TOOLS.contains(&t.as_str()) => {
                    bail!("{t} is opened per operation: asc-mcp profile api-open {name} METHOD PATH")
                }
                Selector::Tool(t) => {
                    let (_, _, tier) = index
                        .iter()
                        .find(|(n, _, _)| n == t)
                        .ok_or_else(|| anyhow!("no tool '{t}' (see asc-mcp tools)"))?;
                    if *tier == Tier::Danger && !flags.dangerous {
                        bail!("{t} is danger tier: add --dangerous if you mean it");
                    }
                }
                Selector::Tier(Tier::Danger) => {
                    bail!("tier:danger cannot be opened as a whole; open each tool by name with --dangerous")
                }
                _ => {}
            }
            let until = until_from(&flags)?;
            let mut file = read_or_empty(&path)?;
            let profile = file.profiles.entry(name.clone()).or_default();
            profile.open.retain(|g| g.select.trim() != raw);
            profile.open.push(Grant {
                select: raw.to_string(),
                until,
                dangerous: flags.dangerous,
            });
            if profile.deny.iter().any(|d| d.trim() == raw) {
                println!("note: '{raw}' is also in deny, which wins; remove it from deny by hand");
            }
            let snapshot = profile.clone();
            write_atomic(&path, &file)?;
            report_selector(name, &snapshot, &selector, &index);
            Ok(())
        }
        "close" => {
            need(3, "close NAME SELECTOR")?;
            let (name, raw) = (&pos[1], pos[2].trim());
            let mut file = read_or_empty(&path)?;
            let profile = file
                .profiles
                .get_mut(name)
                .ok_or_else(|| anyhow!("no profile '{name}'"))?;
            let before = profile.open.len();
            profile.open.retain(|g| g.select.trim() != raw);
            if profile.open.len() == before {
                println!("'{raw}' was not opened by name in {name}");
            }
            let snapshot = profile.clone();
            write_atomic(&path, &file)?;
            if let Some(selector) = Selector::parse(raw) {
                report_selector(name, &snapshot, &selector, &AppStoreServer::tool_index());
            }
            Ok(())
        }
        "api-open" => {
            need(4, "api-open NAME METHOD PATH [--for 2h|--until TS] [--dangerous]")?;
            let (name, method, pattern) = (&pos[1], pos[2].to_ascii_uppercase(), pos[3].trim());
            if !METHODS.contains(&method.as_str()) {
                bail!("METHOD is one of {}", METHODS.join(", "));
            }
            if method == "DELETE" && !flags.dangerous {
                bail!("DELETE needs --dangerous");
            }
            if !pattern.starts_with("/v") || pattern.contains("..") || pattern.contains('%') {
                bail!("PATH is like /v1/apps/*/customerReviews");
            }
            let until = until_from(&flags)?;
            let mut file = read_or_empty(&path)?;
            let profile = file.profiles.entry(name.clone()).or_default();
            profile
                .api
                .retain(|g| !(g.method.eq_ignore_ascii_case(&method) && g.path.trim() == pattern));
            profile.api.push(ApiGrant {
                method: method.clone(),
                path: pattern.to_string(),
                until,
                dangerous: flags.dangerous,
            });
            write_atomic(&path, &file)?;
            println!("{name}: {method} {pattern} open{}", if method == "GET" { "" } else { ", needs confirm" });
            Ok(())
        }
        "api-close" => {
            need(4, "api-close NAME METHOD PATH")?;
            let (name, method, pattern) = (&pos[1], pos[2].to_ascii_uppercase(), pos[3].trim());
            let mut file = read_or_empty(&path)?;
            let profile = file
                .profiles
                .get_mut(name)
                .ok_or_else(|| anyhow!("no profile '{name}'"))?;
            let before = profile.api.len();
            profile
                .api
                .retain(|g| !(g.method.eq_ignore_ascii_case(&method) && g.path.trim() == pattern));
            if profile.api.len() == before {
                println!("{method} {pattern} was not open in {name}");
            }
            write_atomic(&path, &file)?;
            println!("{name}: {method} {pattern} closed");
            Ok(())
        }
        other => bail!("unknown profile command '{other}'\n\n{USAGE}"),
    }
}

/// After open/close: the effective state of what the selector names.
fn report_selector(name: &str, profile: &Profile, selector: &Selector, index: &[(String, Group, Tier)]) {
    let now = now_unix();
    let mut lines = 0;
    for (tool, group, tier) in index {
        let named = match selector {
            Selector::Tool(t) => t == tool,
            Selector::Group(g) => g == group,
            Selector::Tier(t) => t == tier,
        };
        if named {
            println!("{name}: {tool} ({}) {}", tier.as_str(), describe(profile.tool_access(tool, *group, *tier, now)));
            lines += 1;
        }
    }
    if lines == 0 {
        println!("{name}: selector names no tool");
    }
}

fn show(name: &str, p: &Profile) {
    let now = now_unix();
    if let Some(note) = &p.note {
        println!("note: {note}");
    }
    println!("confirm release/danger: {}", p.confirm);
    for g in &p.open {
        println!(
            "open  {}{}{}",
            g.select,
            g.until.as_deref().map(|u| format!(" until {u}")).unwrap_or_default(),
            if g.dangerous { " (dangerous)" } else { "" }
        );
    }
    for d in &p.deny {
        println!("deny  {d}");
    }
    for a in &p.api {
        println!(
            "api   {} {}{}{}",
            a.method.to_ascii_uppercase(),
            a.path,
            a.until.as_deref().map(|u| format!(" until {u}")).unwrap_or_default(),
            if a.dangerous { " (dangerous)" } else { "" }
        );
    }
    let open: Vec<String> = AppStoreServer::tool_index()
        .into_iter()
        .filter_map(|(tool, group, tier)| match p.tool_access(&tool, group, tier, now) {
            Access::Closed => None,
            a => Some(format!("  {tool} ({}) {}", tier.as_str(), describe(a))),
        })
        .collect();
    println!("{name}: {} tools open now", open.len());
    for line in open {
        println!("{line}");
    }
}

/// Problems a hand-edited file may have.
fn check(file: &ProfileFile) -> Vec<String> {
    let index = AppStoreServer::tool_index();
    let mut out = Vec::new();
    for (name, p) in &file.profiles {
        for g in &p.open {
            match Selector::parse(&g.select) {
                None => out.push(format!("{name}: bad selector '{}'", g.select)),
                Some(Selector::Tool(t)) if API_TOOLS.contains(&t.as_str()) => {
                    out.push(format!("{name}: {t} is opened with api entries, not open"))
                }
                Some(Selector::Tool(t)) => match index.iter().find(|(n, _, _)| *n == t) {
                    None => out.push(format!("{name}: no tool '{t}'")),
                    Some((_, _, Tier::Danger)) if !g.dangerous => {
                        out.push(format!("{name}: {t} is danger tier and stays closed without \"dangerous\": true"))
                    }
                    _ => {}
                },
                Some(Selector::Tier(Tier::Danger)) => {
                    out.push(format!("{name}: tier:danger opens nothing; name each tool"))
                }
                Some(_) => {}
            }
            if let Some(u) = &g.until {
                if parse_utc(u).is_none() {
                    out.push(format!("{name}: bad until '{u}' (closed)"));
                }
            }
        }
        for d in &p.deny {
            if Selector::parse(d).is_none() {
                out.push(format!("{name}: bad deny '{d}'"));
            }
        }
        for a in &p.api {
            if !METHODS.contains(&a.method.to_ascii_uppercase().as_str()) {
                out.push(format!("{name}: bad method '{}'", a.method));
            }
            if a.method.eq_ignore_ascii_case("DELETE") && !a.dangerous {
                out.push(format!("{name}: DELETE {} stays closed without \"dangerous\": true", a.path));
            }
            if let Some(u) = &a.until {
                if parse_utc(u).is_none() {
                    out.push(format!("{name}: bad until '{u}' (closed)"));
                }
            }
        }
    }
    out
}
