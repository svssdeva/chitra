//! `chitra install` — register the MCP server with the coding agents on this machine.
//!
//! chitra speaks MCP over stdio, so any client can already use it — but "any
//! client can" is not the same as "it took one command". Both upstreams ship a
//! registrar (graphify covers 19 platforms, code-review-graph 16) and chitra
//! asking people to hand-edit JSON gave back the install-friction advantage it
//! won the Phase 2 gate on.
//!
//! Config paths and key names here are not guessed: they were read out of
//! code-review-graph's shipping installer, which is the closest thing to a
//! tested source of truth for a moving target.
//!
//! Merging is strict about one thing: **never remove or rewrite another
//! server's entry.** These files hold a user's whole agent setup.

use anyhow::{anyhow, Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

/// Where a platform keeps its MCP config, and under which key.
struct Platform {
    id: &'static str,
    name: &'static str,
    /// Config location. `Project` files live in the repo; `Home` ones are global.
    scope: Scope,
    /// Path relative to the repo root, or to the home directory.
    rel: &'static str,
    /// `mcpServers` for JSON platforms, `mcp_servers` for Codex's TOML.
    key: &'static str,
    format: Format,
    /// Some clients require an explicit transport type; others reject it.
    needs_type: bool,
}

#[derive(PartialEq, Clone, Copy)]
enum Scope {
    Project,
    Home,
}

#[derive(PartialEq, Clone, Copy)]
enum Format {
    Json,
    Toml,
}

const PLATFORMS: &[Platform] = &[
    Platform {
        id: "claude",
        name: "Claude Code",
        scope: Scope::Project,
        rel: ".mcp.json",
        key: "mcpServers",
        format: Format::Json,
        needs_type: true,
    },
    Platform {
        id: "codex",
        name: "Codex",
        scope: Scope::Home,
        rel: ".codex/config.toml",
        key: "mcp_servers",
        format: Format::Toml,
        needs_type: true,
    },
    Platform {
        id: "cursor",
        name: "Cursor",
        scope: Scope::Project,
        rel: ".cursor/mcp.json",
        key: "mcpServers",
        format: Format::Json,
        needs_type: true,
    },
    Platform {
        id: "antigravity",
        name: "Antigravity",
        scope: Scope::Home,
        rel: ".gemini/antigravity/mcp_config.json",
        key: "mcpServers",
        format: Format::Json,
        needs_type: false,
    },
    Platform {
        id: "windsurf",
        name: "Windsurf",
        scope: Scope::Home,
        rel: ".codeium/windsurf/mcp_config.json",
        key: "mcpServers",
        format: Format::Json,
        needs_type: false,
    },
    Platform {
        id: "vscode",
        name: "VS Code / Copilot",
        scope: Scope::Project,
        rel: ".vscode/mcp.json",
        key: "mcpServers",
        format: Format::Json,
        needs_type: true,
    },
    Platform {
        id: "claude-desktop",
        name: "Claude Desktop",
        scope: Scope::Home,
        rel: CLAUDE_DESKTOP_REL,
        key: "mcpServers",
        format: Format::Json,
        needs_type: false,
    },
];

/// The only config path that is not the same relative to `$HOME` on every OS —
/// Claude Desktop follows each platform's own application-data convention.
#[cfg(windows)]
const CLAUDE_DESKTOP_REL: &str = "AppData/Roaming/Claude/claude_desktop_config.json";
#[cfg(target_os = "macos")]
const CLAUDE_DESKTOP_REL: &str = "Library/Application Support/Claude/claude_desktop_config.json";
#[cfg(not(any(windows, target_os = "macos")))]
const CLAUDE_DESKTOP_REL: &str = ".config/Claude/claude_desktop_config.json";

/// `std::fs::canonicalize` on Windows returns a `\\?\` extended-length path.
/// It is valid, but it leaks into config files an agent has to launch from, and
/// not every client tolerates it.
///
/// The separator rewrite is Windows-only on purpose: a backslash is a legal
/// character in a Unix filename, so rewriting it there would corrupt the path
/// rather than normalize it.
pub fn plain(p: &Path) -> String {
    #[cfg(windows)]
    {
        let s = p.to_string_lossy().replace('\\', "/");
        s.strip_prefix("//?/").unwrap_or(&s).to_string()
    }
    #[cfg(not(windows))]
    {
        p.to_string_lossy().into_owned()
    }
}

fn home() -> Result<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .ok_or_else(|| anyhow!("cannot locate the home directory"))
}

impl Platform {
    fn config_path(&self, root: &Path) -> Result<PathBuf> {
        Ok(match self.scope {
            Scope::Project => root.join(self.rel),
            Scope::Home => home()?.join(self.rel),
        })
    }

    /// A platform counts as present when its config directory already exists —
    /// creating one for a tool that is not installed would be litter.
    fn detected(&self, root: &Path) -> bool {
        match self.config_path(root) {
            Ok(p) => p.exists() || p.parent().is_some_and(|d| d.exists()),
            Err(_) => false,
        }
    }
}

/// The server entry every platform gets, modulo the `type` key.
fn entry(exe: &str, db: &str, root: &Path, needs_type: bool) -> Value {
    let mut m = Map::new();
    if needs_type {
        m.insert("type".into(), json!("stdio"));
    }
    m.insert("command".into(), json!(exe));
    // Both paths are absolute: an agent launches the server from whatever
    // directory it likes, so a relative --db would resolve somewhere else.
    m.insert(
        "args".into(),
        json!(["serve", "--db", db, "--root", plain(root)]),
    );
    Value::Object(m)
}

/// Insert `chitra` under `key`, leaving every other server untouched.
fn merge_json(existing: &str, key: &str, server: &Value) -> Result<String> {
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(existing).context("existing config is not valid JSON")?
    };
    if !root.is_object() {
        return Err(anyhow!("existing config is not a JSON object"));
    }
    root.as_object_mut()
        .expect("checked above")
        .entry(key)
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("`{key}` in the existing config is not an object"))?
        .insert("chitra".into(), server.clone());
    Ok(serde_json::to_string_pretty(&root)? + "\n")
}

/// Same for Codex's TOML. `toml_edit` is used so the user's comments, ordering
/// and formatting survive — rewriting someone's whole config to add four lines
/// would be rude.
fn merge_toml(existing: &str, key: &str, server: &Value) -> Result<String> {
    let mut doc: toml_edit::DocumentMut = existing
        .parse()
        .context("existing Codex config is not valid TOML")?;
    let table = doc[key].or_insert(toml_edit::table());
    table
        .as_table_like_mut()
        .ok_or_else(|| anyhow!("`{key}` in the Codex config is not a table"))?
        .insert("chitra", json_to_toml(server)?);
    Ok(doc.to_string())
}

fn json_to_toml(v: &Value) -> Result<toml_edit::Item> {
    Ok(match v {
        Value::String(s) => toml_edit::value(s.as_str()),
        Value::Array(a) => {
            let mut arr = toml_edit::Array::new();
            for x in a {
                arr.push(x.as_str().ok_or_else(|| anyhow!("expected string"))?);
            }
            toml_edit::value(arr)
        }
        Value::Object(o) => {
            let mut t = toml_edit::Table::new();
            for (k, val) in o {
                t.insert(k, json_to_toml(val)?);
            }
            toml_edit::Item::Table(t)
        }
        other => return Err(anyhow!("cannot express {other} in TOML")),
    })
}

pub struct Options {
    pub root: PathBuf,
    pub db: String,
    pub only: Option<String>,
    pub dry_run: bool,
}

pub fn run(opts: &Options) -> Result<()> {
    let exe =
        plain(&std::env::current_exe().context("cannot determine the chitra executable path")?);

    let targets: Vec<&Platform> = match &opts.only {
        Some(id) if id != "all" => {
            let p = PLATFORMS.iter().find(|p| p.id == id).ok_or_else(|| {
                anyhow!(
                    "unknown platform `{id}`; known: {}",
                    PLATFORMS
                        .iter()
                        .map(|p| p.id)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            })?;
            vec![p]
        }
        Some(_) => PLATFORMS.iter().collect(),
        None => PLATFORMS
            .iter()
            .filter(|p| p.detected(&opts.root))
            .collect(),
    };

    if targets.is_empty() {
        println!(
            "No supported agent found on this machine.\n\
             Pick one explicitly: chitra install --platform <{}>",
            PLATFORMS.iter().map(|p| p.id).collect::<Vec<_>>().join("|")
        );
        return Ok(());
    }

    for p in targets {
        let path = p.config_path(&opts.root)?;
        let server = entry(&exe, &opts.db, &opts.root, p.needs_type);
        let existing = std::fs::read_to_string(&path).unwrap_or_default();
        let merged = match p.format {
            Format::Json => merge_json(&existing, p.key, &server),
            Format::Toml => merge_toml(&existing, p.key, &server),
        };
        let merged = match merged {
            Ok(m) => m,
            // One unparseable config must not stop the others, and must never be
            // overwritten — that would destroy a user's agent setup.
            Err(e) => {
                eprintln!("skipped {} ({}): {e}", p.name, plain(&path));
                continue;
            }
        };

        if opts.dry_run {
            println!("--- {} · {} ---\n{merged}", p.name, path.display());
            continue;
        }
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, merged).with_context(|| format!("writing {}", path.display()))?;
        println!("{:<18} {}", p.name, plain(&path));
    }

    if !opts.dry_run {
        println!("\nRestart the agent to pick up the server, then ask it to use `chitra`.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server() -> Value {
        entry("/bin/chitra", ".chitra/graph.db", Path::new("/repo"), true)
    }

    #[test]
    fn writes_into_an_empty_config() {
        let out = merge_json("", "mcpServers", &server()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["chitra"]["command"], "/bin/chitra");
        assert_eq!(v["mcpServers"]["chitra"]["args"][0], "serve");
        assert_eq!(v["mcpServers"]["chitra"]["type"], "stdio");
    }

    /// The one thing this must never do.
    #[test]
    fn leaves_other_servers_alone() {
        let before = r#"{"mcpServers":{"other":{"command":"x"}},"theme":"dark"}"#;
        let out = merge_json(before, "mcpServers", &server()).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["mcpServers"]["other"]["command"], "x");
        assert_eq!(v["theme"], "dark");
        assert!(v["mcpServers"]["chitra"].is_object());
    }

    #[test]
    fn running_twice_changes_nothing_further() {
        let once = merge_json("", "mcpServers", &server()).unwrap();
        let twice = merge_json(&once, "mcpServers", &server()).unwrap();
        assert_eq!(once, twice);
    }

    #[test]
    fn a_broken_config_is_refused_not_overwritten() {
        assert!(merge_json("{not json", "mcpServers", &server()).is_err());
        assert!(merge_json("[1,2,3]", "mcpServers", &server()).is_err());
    }

    #[test]
    fn toml_keeps_the_users_comments_and_other_servers() {
        let before = "# my codex setup\nmodel = \"o3\"\n\n[mcp_servers.other]\ncommand = \"x\"\n";
        let out = merge_toml(before, "mcp_servers", &server()).unwrap();
        assert!(out.contains("# my codex setup"), "comment lost:\n{out}");
        assert!(out.contains("model = \"o3\""));
        assert!(out.contains("[mcp_servers.other]"));
        assert!(
            out.contains("[mcp_servers.chitra]"),
            "chitra missing:\n{out}"
        );
        assert!(out.contains("serve"));
    }

    #[test]
    fn toml_is_idempotent() {
        let once = merge_toml("", "mcp_servers", &server()).unwrap();
        let twice = merge_toml(&once, "mcp_servers", &server()).unwrap();
        assert_eq!(once, twice);
    }

    /// Windows canonicalisation returns an extended-length path. It must
    /// not leak into a config an agent has to launch the server from.
    #[test]
    fn verbatim_windows_prefix_is_stripped() {
        let verbatim = String::from("\\\\?\\C:\\repo\\src");
        assert_eq!(plain(Path::new(&verbatim)), "C:/repo/src");
        assert_eq!(plain(Path::new("/home/u/repo")), "/home/u/repo");
    }
    #[test]
    fn windsurf_style_platforms_omit_the_type_key() {
        let e = entry("/bin/chitra", "g.db", Path::new("/repo"), false);
        assert!(e.get("type").is_none());
    }
}
