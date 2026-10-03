//! `rpi mcp`: add, remove, and check MCP servers and sign in to them outside
//! a session (port of `packages/coding-agent/src/extensions/mcp/cli.ts` @
//! a13d35a74). Agents run it through bash to configure servers, verify an
//! `mcp.json` they wrote, and start an OAuth sign-in; the user only approves
//! access in the browser. Running sessions pick up new credentials on their
//! next turn.

use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::{Map, Value, json};
use tokio_util::sync::CancellationToken;

use crate::extensions::mcp::config::{
    CONFIG_DIR_NAME, McpScope, McpServerConfig, McpServerEntry, add_mcp_server_config,
    get_mcp_tool_exposure, load_mcp_config, remove_mcp_server_config, validate_mcp_server_config,
};
use crate::extensions::mcp::oauth::{McpOAuthCredentialStore, McpSignInPrompt, sign_in_mcp_server};
use crate::extensions::mcp::runtime::{
    McpServerConnection, McpServerConnectionOptions, ServerState, create_default_transport,
};

/// `new ProjectTrustStore(agentDir).get(cwd) === true` (cli.ts:264).
fn project_trusted(agent_dir: &Path, cwd: &str) -> bool {
    crate::core::trust_manager::ProjectTrustStore::new(agent_dir)
        .get(Path::new(cwd))
        .map(|value| value == Some(true))
        .unwrap_or(false)
}

const APP_NAME: &str = "rpi";
const DEFAULT_LOGIN_TIMEOUT_SECONDS: u64 = 300;

const HELP: &str = "Usage:
  rpi mcp add <server> [options] -- <command> [args...]
  rpi mcp add <server> [options] --url <url>
  rpi mcp remove <server> [-l]
  rpi mcp list [--json]
  rpi mcp login <server> [--timeout <seconds>]
  rpi mcp logout <server>

Configure and check MCP servers and sign in to OAuth servers without starting a session.
Reads ~/.rpi/agent/mcp.json and, in trusted projects, .rpi/mcp.json.

Commands:
  add <server>            Add or replace a server in mcp.json
  remove <server>         Remove a server from mcp.json
  list                    Show state, tools, and errors (exits 1 on failure)
  login <server>          Sign in through the browser
  logout <server>         Delete the stored OAuth credentials

Options for add and remove:
  -l, --local             Use .rpi/mcp.json in the current project instead of the global file

Options for add:
  --url <url>             Streamable HTTP server URL (instead of a command)
  --env <KEY=VALUE>       Environment variable for a stdio server (repeatable)
  --cwd <dir>             Working directory for a stdio server
  --header <KEY=VALUE>    HTTP header (repeatable)
  --bearer-token-env-var <NAME>
                          Send \"Authorization: Bearer ${NAME}\"
  --oauth-client-id <id>  Pre-registered OAuth client id
  --oauth-client-secret <secret>
                          OAuth client secret (may be ${NAME} or !command)
  --oauth-callback-port <port>
                          Fixed OAuth callback port
  --oauth-client-name <name>
                          Client name sent when registering with the OAuth server
  --exposure <mode>       codemode (default), deferred, direct, or hidden
  --description <text>    What the server offers, shown in the system prompt

Other options:
  --json                  Print the list as JSON
  --timeout <seconds>     How long login waits for the browser (default: 300)";

const HELP_HINT: &str = "Use \"rpi mcp --help\" for usage.";

/// `McpCommandOptions` (cli.ts:88).
pub struct McpCommandOptions {
    pub cwd: String,
    pub agent_dir: PathBuf,
    pub credentials: Option<Arc<McpOAuthCredentialStore>>,
    pub open_url: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    pub log: Arc<dyn Fn(&str) + Send + Sync>,
    pub error: Arc<dyn Fn(&str) + Send + Sync>,
}

impl McpCommandOptions {
    fn log(&self, line: impl AsRef<str>) {
        (self.log)(line.as_ref());
    }

    fn error(&self, line: impl AsRef<str>) {
        (self.error)(line.as_ref());
    }
}

/// Parse `--name value` options (`parseOptions`, cli.ts:176).
#[derive(Default)]
struct ParsedOptions {
    positional: Vec<String>,
    values: std::collections::HashMap<String, String>,
    flags: std::collections::HashSet<String>,
    lists: std::collections::HashMap<String, Vec<String>>,
}

fn parse_options(
    args: &[String],
    known: &[(&str, Option<char>)],
    max_positionals: Option<usize>,
    options: &McpCommandOptions,
) -> Option<ParsedOptions> {
    let mut parsed = ParsedOptions::default();
    let mut index = 0;
    while index < args.len() {
        let mut arg = args[index].clone();
        if arg == "-l" {
            arg = "--local".to_owned();
        }
        if arg == "--" || max_positionals.is_some_and(|max| parsed.positional.len() >= max) {
            let rest = if arg == "--" {
                &args[index + 1..]
            } else {
                &args[index..]
            };
            parsed.positional.extend(rest.iter().cloned());
            break;
        }
        if !arg.starts_with("--") {
            parsed.positional.push(arg);
            index += 1;
            continue;
        }
        let name = arg.trim_start_matches("--").to_owned();
        let Some((canonical, kind)) = known.iter().find(|(canonical, _)| *canonical == name) else {
            options.error(format!("Unknown option {arg}.\n{HELP_HINT}"));
            return None;
        };
        let canonical = (*canonical).to_owned();
        match kind {
            None => {
                parsed.flags.insert(canonical);
                index += 1;
            }
            Some(_kind) => {
                index += 1;
                let Some(value) = args.get(index) else {
                    options.error(format!("--{canonical} needs a value."));
                    return None;
                };
                // "list" kind is marked with a marker char in `known`.
                if matches!(kind, Some('*')) {
                    parsed
                        .lists
                        .entry(canonical)
                        .or_default()
                        .push(value.clone());
                } else {
                    parsed.values.insert(canonical, value.clone());
                }
                index += 1;
            }
        }
    }
    Some(parsed)
}

fn parse_pairs(
    option: &str,
    pairs: Option<&Vec<String>>,
    options: &McpCommandOptions,
) -> Option<Map<String, Value>> {
    let mut record = Map::new();
    for pair in pairs.into_iter().flatten() {
        let Some((key, value)) = pair.split_once('=') else {
            options.error(format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
            return None;
        };
        if key.is_empty() {
            options.error(format!("--{option} expects KEY=VALUE, got \"{pair}\"."));
            return None;
        }
        record.insert(key.to_owned(), Value::String(value.to_owned()));
    }
    Some(record)
}

fn describe_transport(entry: &McpServerEntry) -> String {
    match &entry.config {
        McpServerConfig::Http(config) => config.url.clone(),
        McpServerConfig::Stdio(config) => {
            let mut parts = vec![config.command.clone()];
            parts.extend(config.args.clone().unwrap_or_default());
            parts.join(" ")
        }
    }
}

/// `runMcpCommand` (cli.ts:238): the `rpi mcp <args>` entry point.
pub async fn run_mcp_command(args: &[String], options: McpCommandOptions) -> i32 {
    let command = args.first().cloned();
    if command.is_none()
        || command.as_deref() == Some("help")
        || args.iter().any(|arg| arg == "--help" || arg == "-h")
    {
        options.log(HELP);
        return 0;
    }
    let command = command.expect("checked");
    let rest = &args[1..];
    let project_config = Path::new(&options.cwd)
        .join(CONFIG_DIR_NAME)
        .join("mcp.json");
    if command == "add" || command == "remove" {
        return if command == "add" {
            add(rest, &project_config, &options)
        } else {
            remove(rest, &project_config, &options)
        };
    }
    let project_trusted = project_trusted(&options.agent_dir, &options.cwd);
    let loaded = load_mcp_config(&options.agent_dir, Path::new(&options.cwd), project_trusted);
    let untrusted_note = (!project_trusted && project_config.exists()).then(|| {
        format!(
            "{} is ignored because the project is not trusted. Start {APP_NAME} in the project to trust it.",
            project_config.display()
        )
    });
    let credentials = options
        .credentials
        .clone()
        .unwrap_or_else(|| Arc::new(McpOAuthCredentialStore::new(&options.agent_dir)));

    match command.as_str() {
        "list" => {
            let Some(parsed) = parse_options(rest, &[("json", None)], None, &options) else {
                return 1;
            };
            if !parsed.positional.is_empty() {
                options.error(format!("Usage: {APP_NAME} mcp list [--json]\n{HELP_HINT}"));
                return 1;
            }
            list(
                &loaded,
                parsed.flags.contains("json"),
                untrusted_note.as_deref(),
                &options,
                credentials,
            )
            .await
        }
        "login" | "logout" => {
            let known: &[(&str, Option<char>)] = if command == "login" {
                &[("timeout", Some('v'))]
            } else {
                &[]
            };
            let Some(parsed) = parse_options(rest, known, None, &options) else {
                return 1;
            };
            let Some(name) = parsed.positional.first().cloned() else {
                options.error(format!(
                    "Usage: {APP_NAME} mcp {command} <server>\n{HELP_HINT}"
                ));
                return 1;
            };
            if parsed.positional.len() > 1 {
                options.error(format!(
                    "Usage: {APP_NAME} mcp {command} <server>\n{HELP_HINT}"
                ));
                return 1;
            }
            let Some(entry) = loaded
                .servers
                .iter()
                .find(|server| server.name == name)
                .cloned()
            else {
                let names = loaded
                    .servers
                    .iter()
                    .map(|server| server.name.clone())
                    .collect::<Vec<_>>()
                    .join(", ");
                options.error(format!(
                    "No MCP server named \"{name}\".{}{} Configured: {}.",
                    untrusted_note
                        .as_deref()
                        .map(|note| format!(" {note}"))
                        .unwrap_or_default(),
                    if untrusted_note.is_some() { "" } else { "" },
                    if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names
                    }
                ));
                return 1;
            };
            let connection = create_connection(&entry, &options, credentials.clone());
            let Some(url) = connection.oauth_url() else {
                options.error(format!(
                    "MCP server \"{name}\" does not use OAuth. Only HTTP servers without an Authorization header do."
                ));
                return 1;
            };
            if command == "logout" {
                let removed = credentials.remove(&name, &url);
                options.log(if removed {
                    format!("Signed out of MCP server \"{name}\".")
                } else {
                    format!("No stored credentials for MCP server \"{name}\".")
                });
                return 0;
            }
            let timeout = parsed
                .values
                .get("timeout")
                .and_then(|value| value.parse::<f64>().ok())
                .unwrap_or(DEFAULT_LOGIN_TIMEOUT_SECONDS as f64);
            if !timeout.is_finite() || timeout <= 0.0 {
                options.error("--timeout must be a positive number of seconds.");
                return 1;
            }
            let code = login(
                &entry,
                &connection,
                &url,
                (timeout * 1000.0) as u64,
                &options,
                credentials,
            )
            .await;
            connection.close().await;
            code
        }
        other => {
            options.error(format!("Unknown mcp command \"{other}\".\n{HELP_HINT}"));
            1
        }
    }
}

fn create_connection(
    entry: &McpServerEntry,
    options: &McpCommandOptions,
    credentials: Arc<McpOAuthCredentialStore>,
) -> Arc<McpServerConnection> {
    Arc::new(McpServerConnection::new(McpServerConnectionOptions {
        entry: entry.clone(),
        cwd: options.cwd.clone(),
        create_transport: Arc::new(|entry, cwd, auth| create_default_transport(entry, cwd, auth)),
        credentials,
        provider_token: None,
        on_tools: Arc::new(|_| {}),
        on_change: None,
        log: Some(Arc::new(crate::extensions::mcp::log::McpServerLog::new(
            options.agent_dir.join("mcp.log"),
        ))),
    }))
}

/// `add` (cli.ts:406).
fn add(args: &[String], project_config: &Path, options: &McpCommandOptions) -> i32 {
    let usage = format!(
        "Usage: {APP_NAME} mcp add <server> [options] (--url <url> | -- <command> [args...])\n{HELP_HINT}"
    );
    let known = [
        ("local", None),
        ("url", Some('v')),
        ("env", Some('*')),
        ("cwd", Some('v')),
        ("header", Some('*')),
        ("bearer-token-env-var", Some('v')),
        ("oauth-client-id", Some('v')),
        ("oauth-client-secret", Some('v')),
        ("oauth-callback-port", Some('v')),
        ("oauth-client-name", Some('v')),
        ("exposure", Some('v')),
        ("description", Some('v')),
    ];
    let Some(parsed) = parse_options(args, &known, Some(2), options) else {
        return 1;
    };
    let Some(name) = parsed.positional.first().cloned() else {
        options.error(usage);
        return 1;
    };
    let command: Vec<String> = parsed.positional.iter().skip(1).cloned().collect();
    let url = parsed.values.get("url").cloned();
    if (url.is_none()) == command.is_empty() {
        options.error(usage);
        return 1;
    }
    let value = |option: &str| parsed.values.get(option).cloned();
    let http_only = [
        "header",
        "bearer-token-env-var",
        "oauth-client-id",
        "oauth-client-secret",
        "oauth-callback-port",
        "oauth-client-name",
    ];
    let stdio_only = ["env", "cwd"];
    let misplaced = if url.is_none() {
        &http_only[..]
    } else {
        &stdio_only[..]
    }
    .iter()
    .find(|option| parsed.values.contains_key(**option) || parsed.lists.contains_key(**option));
    if let Some(misplaced) = misplaced {
        options.error(format!(
            "--{misplaced} only applies to {}.",
            if url.is_none() {
                "HTTP servers (--url)"
            } else {
                "stdio servers"
            }
        ));
        return 1;
    }
    let mut config = Map::new();
    if let Some(url) = &url {
        let Some(headers) = parse_pairs("header", parsed.lists.get("header"), options) else {
            return 1;
        };
        let bearer = value("bearer-token-env-var");
        if let Some(bearer) = bearer {
            config.insert("headers".to_owned(), {
                let mut map = headers.clone();
                map.insert(
                    "Authorization".to_owned(),
                    Value::String(format!("Bearer ${{{bearer}}}")),
                );
                Value::Object(map)
            });
        } else if !headers.is_empty() {
            config.insert("headers".to_owned(), Value::Object(headers));
        }
        let mut oauth = Map::new();
        if let Some(client_id) = value("oauth-client-id") {
            oauth.insert("clientId".to_owned(), Value::String(client_id));
        }
        if let Some(client_secret) = value("oauth-client-secret") {
            oauth.insert("clientSecret".to_owned(), Value::String(client_secret));
        }
        if let Some(port) = value("oauth-callback-port") {
            let Ok(port) = port.parse::<u64>() else {
                options.error("--oauth-callback-port expects a port number.");
                return 1;
            };
            oauth.insert("callbackPort".to_owned(), Value::Number(port.into()));
        }
        if let Some(client_name) = value("oauth-client-name") {
            oauth.insert("clientName".to_owned(), Value::String(client_name));
        }
        if !oauth.is_empty() {
            config.insert("oauth".to_owned(), Value::Object(oauth));
        }
        config.insert("url".to_owned(), Value::String(url.clone()));
    } else {
        let Some(env) = parse_pairs("env", parsed.lists.get("env"), options) else {
            return 1;
        };
        let executable = command.first().cloned().unwrap_or_default();
        config.insert("command".to_owned(), Value::String(executable));
        if command.len() > 1 {
            config.insert(
                "args".to_owned(),
                Value::Array(
                    command[1..]
                        .iter()
                        .map(|arg| Value::String(arg.clone()))
                        .collect(),
                ),
            );
        }
        if !env.is_empty() {
            config.insert("env".to_owned(), Value::Object(env));
        }
        if let Some(cwd) = value("cwd") {
            config.insert("cwd".to_owned(), Value::String(cwd));
        }
    }
    if let Some(exposure) = value("exposure") {
        config.insert("exposure".to_owned(), Value::String(exposure));
    }
    if let Some(description) = value("description") {
        config.insert("description".to_owned(), Value::String(description));
    }
    let validated = match validate_mcp_server_config(&name, &Value::Object(config)) {
        Ok(config) => config,
        Err(error) => {
            options.error(error);
            return 1;
        }
    };
    let project = parsed.flags.contains("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        options.agent_dir.join("mcp.json")
    };
    let scope = if project { "project" } else { "global" };
    match add_mcp_server_config(&path, &name, &validated) {
        Ok(replaced) => {
            options.log(format!(
                "{} {scope} MCP server \"{name}\" in {}.",
                if replaced { "Replaced" } else { "Added" },
                path.display()
            ));
        }
        Err(error) => {
            options.error(format!("Could not update {}: {error}", path.display()));
            return 1;
        }
    }
    if project && !project_trusted(&options.agent_dir, &options.cwd) {
        options.log(format!(
            "The project is not trusted, so {} is ignored until you start {APP_NAME} in the project and trust it.",
            path.display()
        ));
    }
    // HTTP servers without an Authorization header may use OAuth.
    let may_need_sign_in = validated.url().is_some()
        && !matches!(&validated, McpServerConfig::Http(config)
            if config.headers.clone().unwrap_or_default().keys().any(|header| header.eq_ignore_ascii_case("authorization")));
    options.log(format!(
        "Check it with: {APP_NAME} mcp list{}",
        if may_need_sign_in {
            format!(". If it requires sign-in: {APP_NAME} mcp login {name}")
        } else {
            String::new()
        }
    ));
    0
}

/// `remove` (cli.ts:521).
fn remove(args: &[String], project_config: &Path, options: &McpCommandOptions) -> i32 {
    let Some(parsed) = parse_options(args, &[("local", None)], None, options) else {
        return 1;
    };
    let Some(name) = parsed.positional.first().cloned() else {
        options.error(format!(
            "Usage: {APP_NAME} mcp remove <server> [-l]\n{HELP_HINT}"
        ));
        return 1;
    };
    if parsed.positional.len() > 1 {
        options.error(format!(
            "Usage: {APP_NAME} mcp remove <server> [-l]\n{HELP_HINT}"
        ));
        return 1;
    }
    let project = parsed.flags.contains("local");
    let path = if project {
        project_config.to_path_buf()
    } else {
        options.agent_dir.join("mcp.json")
    };
    let scope = if project { "project" } else { "global" };
    match remove_mcp_server_config(&path, &name) {
        Ok(true) => {
            options.log(format!(
                "Removed {scope} MCP server \"{name}\" from {}.",
                path.display()
            ));
            return 0;
        }
        Ok(false) => {}
        Err(error) => {
            options.error(format!("Could not update {}: {error}", path.display()));
            return 1;
        }
    }
    let other = load_mcp_config(&options.agent_dir, Path::new(&options.cwd), true)
        .servers
        .into_iter()
        .find(|server| {
            server.name == name
                && server.scope
                    != Some(if project {
                        McpScope::Project
                    } else {
                        McpScope::Global
                    })
        });
    let hint = match other {
        Some(other) => format!(
            " It is defined in {}{}",
            other.source,
            if other.scope == Some(McpScope::Project) {
                "; use --local"
            } else {
                "; omit --local"
            }
        ),
        None => String::new(),
    };
    options.error(format!(
        "No {scope} MCP server named \"{name}\" in {}.{hint}",
        path.display()
    ));
    1
}

/// `ServerReport` (cli.ts:107).
#[derive(Default)]
struct ServerReport {
    name: String,
    scope: String,
    source: String,
    enabled: bool,
    exposure: String,
    transport: String,
    state: String,
    tools: Vec<String>,
    tool_exposure: Map<String, Value>,
    resources: Option<usize>,
    resource_templates: Option<usize>,
    error: Option<String>,
}

/// `list` (cli.ts:558).
async fn list(
    loaded: &crate::extensions::mcp::config::LoadedMcpConfig,
    json_output: bool,
    untrusted_note: Option<&str>,
    options: &McpCommandOptions,
    credentials: Arc<McpOAuthCredentialStore>,
) -> i32 {
    let mut reports: Vec<ServerReport> = Vec::new();
    for entry in loaded.servers.clone() {
        let mut report = ServerReport {
            name: entry.name.clone(),
            scope: entry.scope.unwrap_or(McpScope::Global).as_str().to_owned(),
            source: entry.source.clone(),
            enabled: entry.config.enabled(),
            exposure: entry.config.exposure().as_str().to_owned(),
            transport: describe_transport(&entry),
            state: "disabled".to_owned(),
            ..Default::default()
        };
        if report.enabled {
            let connection = create_connection(&entry, options, credentials.clone());
            let _ = connection.get_client().await;
            report.state = connection.state().as_str().to_owned();
            report.tools = connection
                .tools()
                .into_iter()
                .map(|tool| tool.name)
                .collect();
            let exposure = entry.config.exposure();
            for tool in &report.tools {
                let tool_exposure = get_mcp_tool_exposure(&entry.config, tool);
                if tool_exposure != exposure {
                    report.tool_exposure.insert(
                        tool.clone(),
                        Value::String(tool_exposure.as_str().to_owned()),
                    );
                }
            }
            if connection.has_resources() {
                report.resources = Some(connection.resources().len());
                report.resource_templates = Some(connection.resource_templates().len());
            }
            if connection.state() != ServerState::Connected {
                report.error = connection.error();
            }
            connection.close().await;
        }
        reports.push(report);
    }
    let failed = !loaded.errors.is_empty()
        || reports
            .iter()
            .any(|report| report.enabled && report.state != "connected");
    if json_output {
        let payload = json!({
            "servers": reports.iter().map(|report| {
                let mut value = json!({
                    "name": report.name,
                    "scope": report.scope,
                    "source": report.source,
                    "enabled": report.enabled,
                    "exposure": report.exposure,
                    "transport": report.transport,
                    "state": report.state,
                    "tools": report.tools,
                });
                if !report.tool_exposure.is_empty() {
                    value["toolExposure"] = Value::Object(report.tool_exposure.clone());
                }
                if let Some(resources) = report.resources {
                    value["resources"] = json!(resources);
                    value["resourceTemplates"] = json!(report.resource_templates.unwrap_or(0));
                }
                if let Some(error) = &report.error {
                    value["error"] = json!(error);
                }
                value
            }).collect::<Vec<_>>(),
            "errors": loaded.errors,
            "note": untrusted_note,
        });
        options.log(serde_json::to_string_pretty(&payload).unwrap_or_default());
        return if failed { 1 } else { 0 };
    }
    if reports.is_empty() && loaded.errors.is_empty() {
        options.log(format!(
            "No MCP servers configured. Add them to {} or .rpi/mcp.json.",
            options.agent_dir.join("mcp.json").display()
        ));
    }
    for report in &reports {
        let state = match (report.state.as_str(), report.tools.len()) {
            ("connected", count) => format!(
                "connected, {count} tool{}",
                if count == 1 { "" } else { "s" }
            ),
            ("needs-auth", _) => "needs sign-in".to_owned(),
            (state, _) => state.to_owned(),
        };
        options.log(format!(
            "{}: {state} ({}, {})",
            report.name, report.exposure, report.scope
        ));
        options.log(format!("  {}", report.transport));
        if report.state == "needs-auth" {
            options.log(format!(
                "  sign in with: {APP_NAME} mcp login {}",
                report.name
            ));
        }
        if !report.tools.is_empty() {
            let tools = report
                .tools
                .iter()
                .map(
                    |tool| match report.tool_exposure.get(tool).and_then(Value::as_str) {
                        Some(exposure) => format!("{tool} [{exposure}]"),
                        None => tool.clone(),
                    },
                )
                .collect::<Vec<_>>()
                .join(", ");
            options.log(format!("  tools: {tools}"));
        }
        if let Some(resources) = report.resources {
            options.log(format!(
                "  resources: {resources}, URI templates: {}",
                report.resource_templates.unwrap_or(0)
            ));
        }
        if let Some(error) = &report.error {
            options.log(format!("  {}", error.replace('\n', "\n  ")));
        }
    }
    for error in &loaded.errors {
        options.log(format!("config error: {error}"));
    }
    if let Some(note) = untrusted_note {
        options.log(note);
    }
    if failed { 1 } else { 0 }
}

/// `login` (cli.ts:491).
async fn login(
    entry: &McpServerEntry,
    connection: &Arc<McpServerConnection>,
    url: &str,
    timeout_ms: u64,
    options: &McpCommandOptions,
    credentials: Arc<McpOAuthCredentialStore>,
) -> i32 {
    let name = entry.name.clone();
    // Connecting first answers whether a sign-in is needed and records the
    // server's challenge.
    match connection.get_client().await {
        Ok(_) => {
            options.log(format!(
                "Already signed in to MCP server \"{name}\" ({} tools).",
                connection.tools().len()
            ));
            return 0;
        }
        Err(_) => {
            if connection.state() != ServerState::NeedsAuth {
                options.error(format!(
                    "MCP server \"{name}\" failed to connect: {}",
                    connection
                        .error()
                        .unwrap_or_else(|| "unknown error".to_owned())
                ));
                return 1;
            }
        }
    }
    let interactive = std::io::stdin().is_terminal() && options.open_url.is_none();
    let prompt = CliSignInPrompt {
        options: options.clone_for_prompt(),
        name: name.clone(),
        timeout_ms,
        interactive,
    };
    let store = credentials.for_server(&name, url);
    let result = sign_in_mcp_server(crate::extensions::mcp::oauth::SignInOptions {
        server_url: url.to_owned(),
        store: &store,
        settings: connection.oauth_settings(),
        challenge: connection.challenge(),
        prompt: &prompt,
    })
    .await;
    if let Err(error) = result {
        if error.contains("Sign-in cancelled") {
            options.error(format!(
                "Sign-in to MCP server \"{name}\" was cancelled or not completed within {} seconds.",
                timeout_ms / 1000
            ));
        } else {
            options.error(format!("Sign-in to MCP server \"{name}\" failed: {error}"));
        }
        return 1;
    }
    connection.set_challenge(None);
    if let Err(error) = connection.reconnect().await {
        options.error(format!("Signed in, but {error}"));
        return 1;
    }
    options.log(format!(
        "Signed in to MCP server \"{name}\" ({} tools).",
        connection.tools().len()
    ));
    0
}

impl McpCommandOptions {
    fn clone_for_prompt(&self) -> PromptOptions {
        PromptOptions {
            open_url: self.open_url.clone(),
            log: self.log.clone(),
        }
    }
}

/// The options the CLI sign-in prompt needs (kept separate so the prompt
/// holds no path state).
#[derive(Clone)]
struct PromptOptions {
    open_url: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    log: Arc<dyn Fn(&str) + Send + Sync>,
}

/// The CLI's `McpSignInPrompt` (cli.ts `waitForRedirectUrl`).
struct CliSignInPrompt {
    options: PromptOptions,
    name: String,
    timeout_ms: u64,
    interactive: bool,
}

#[async_trait::async_trait]
impl McpSignInPrompt for CliSignInPrompt {
    fn show_authorization_url(&self, url: url::Url) {
        (self.options.log)(&format!(
            "Sign in to MCP server \"{}\" in your browser:\n{}",
            self.name,
            url.as_str()
        ));
        match &self.options.open_url {
            Some(open) => open(url.as_str()),
            None => open_browser(url.as_str()),
        }
    }

    async fn prompt_for_redirect_url(&self, signal: CancellationToken) -> Option<String> {
        let controller = CancellationToken::new();
        if self.interactive {
            tokio::select! {
                () = signal.cancelled() => None,
                () = tokio::time::sleep(std::time::Duration::from_millis(self.timeout_ms)) => None,
                line = read_line(controller) => line,
            }
        } else {
            tokio::select! {
                () = signal.cancelled() => None,
                () = tokio::time::sleep(std::time::Duration::from_millis(self.timeout_ms)) => None,
            }
        }
    }
}

async fn read_line(signal: CancellationToken) -> Option<String> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::spawn(move || {
        let mut line = String::new();
        let read = std::io::stdin().read_line(&mut line).is_ok();
        let _ = sender.send(if read { Some(line) } else { None });
    });
    tokio::select! {
        result = receiver => result.ok().flatten(),
        () = signal.cancelled() => None,
    }
}

/// Best-effort platform browser opener (upstream `openBrowser`).
fn open_browser(url: &str) {
    #[cfg(target_os = "macos")]
    let command = "open";
    #[cfg(target_os = "windows")]
    let command = "cmd";
    #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
    let command = "xdg-open";
    #[cfg(target_os = "windows")]
    {
        let _ = std::process::Command::new(command)
            .args(["/c", "start", "", url])
            .spawn();
    }
    #[cfg(not(target_os = "windows"))]
    {
        let _ = std::process::Command::new(command).arg(url).spawn();
    }
}

/// Helper for tests and hosts: the default command options with console
/// output.
pub fn console_options(cwd: &str, agent_dir: PathBuf) -> McpCommandOptions {
    McpCommandOptions {
        cwd: cwd.to_owned(),
        agent_dir,
        credentials: None,
        open_url: None,
        log: Arc::new(|line| println!("{line}")),
        error: Arc::new(|line| eprintln!("{line}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_options(dir: &Path) -> McpCommandOptions {
        McpCommandOptions {
            cwd: dir.display().to_string(),
            agent_dir: dir.join("agent"),
            credentials: None,
            open_url: Some(Arc::new(|_| {})),
            log: Arc::new(|_| {}),
            error: Arc::new(|_| {}),
        }
    }

    #[tokio::test]
    async fn add_remove_and_list_without_connecting() {
        let dir = std::env::temp_dir().join(format!("rpi-mcp-cli-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        let options = test_options(&dir);
        let code = run_mcp_command(
            &["add", "echo", "--url", "http://127.0.0.1:1/mcp"]
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            options,
        )
        .await;
        assert_eq!(code, 0);
        let config: Value =
            serde_json::from_str(&std::fs::read_to_string(dir.join("agent/mcp.json")).unwrap())
                .unwrap();
        assert_eq!(
            config["mcpServers"]["echo"]["url"],
            "http://127.0.0.1:1/mcp"
        );

        let options = test_options(&dir);
        let code = run_mcp_command(
            &["add", "local", "--local", "--", "node", "server.js"]
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            options,
        )
        .await;
        assert_eq!(code, 0);
        assert!(dir.join(".rpi/mcp.json").exists());

        let options = test_options(&dir);
        let code = run_mcp_command(
            &["remove", "echo"]
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            options,
        )
        .await;
        assert_eq!(code, 0);
        let options = test_options(&dir);
        let code = run_mcp_command(
            &["remove", "echo"]
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            options,
        )
        .await;
        assert_eq!(code, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn add_rejects_misplaced_and_invalid_options() {
        let dir = std::env::temp_dir().join(format!("rpi-mcp-cli-bad-{}", std::process::id()));
        std::fs::create_dir_all(dir.join("agent")).unwrap();
        let options = test_options(&dir);
        let code = run_mcp_command(
            &["add", "s", "--url", "https://x", "--env", "A=B"]
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            options,
        )
        .await;
        assert_eq!(code, 1);
        let options = test_options(&dir);
        let code = run_mcp_command(
            &["add", "s", "--url", "sse://x"]
                .iter()
                .map(|value| value.to_string())
                .collect::<Vec<_>>(),
            options,
        )
        .await;
        assert_eq!(code, 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
