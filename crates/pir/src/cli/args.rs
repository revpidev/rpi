//! CLI argument parsing and help display.
//!
//! Port of `packages/coding-agent/src/cli/args.ts` @ pi 0.82.1 (2efa728).
//!
//! The parser is hand-rolled (like upstream) rather than clap-based: the
//! pinned semantics — `-p` value swallowing, unknown `--flag` collection into
//! `unknown_flags`, `=` splitting, and diagnostic-graded errors — do not map
//! onto clap's declarative model (deviation from the task-file "clap" note;
//! behavior is the parity contract).
//!
//! Intentional differences (ADR-0001): `APP_NAME` is `pir`, config dir
//! `.pir`, env prefix `PIR_`.

use pir_agent::types::ThinkingLevel;

use crate::cli::diagnostics::Diagnostic;
use crate::config::{APP_NAME, CONFIG_DIR_NAME, ENV_AGENT_DIR, ENV_SESSION_DIR};

/// `Mode = "text" | "json" | "rpc"` (args.ts:10).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Text,
    Json,
    Rpc,
}

impl Mode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Mode::Text => "text",
            Mode::Json => "json",
            Mode::Rpc => "rpc",
        }
    }
}

/// `--list-models` value: bare flag (`true` upstream) or a search pattern
/// (args.ts:46, :171-177).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ListModels {
    All,
    Search(String),
}

/// Value collected for an unknown `--flag` (args.ts:53).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum UnknownFlagValue {
    Boolean(bool),
    String(String),
}

/// `Args` (args.ts:12-55). `Option` fields mirror upstream `undefined`.
#[derive(Debug, Default, Clone)]
pub struct Args {
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_key: Option<String>,
    pub system_prompt: Option<String>,
    pub append_system_prompt: Option<Vec<String>>,
    pub thinking: Option<ThinkingLevel>,
    pub continue_: bool,
    pub resume: bool,
    pub help: bool,
    pub version: bool,
    pub mode: Option<Mode>,
    pub name: Option<String>,
    pub no_session: bool,
    pub session: Option<String>,
    pub session_id: Option<String>,
    pub fork: Option<String>,
    pub session_dir: Option<String>,
    pub models: Option<Vec<String>>,
    pub tools: Option<Vec<String>>,
    pub exclude_tools: Option<Vec<String>>,
    pub no_tools: bool,
    pub no_builtin_tools: bool,
    pub extensions: Option<Vec<String>>,
    pub no_extensions: bool,
    pub print: bool,
    pub export: Option<String>,
    pub no_skills: bool,
    pub skills: Option<Vec<String>>,
    pub prompt_templates: Option<Vec<String>>,
    pub no_prompt_templates: bool,
    pub themes: Option<Vec<String>>,
    pub no_themes: bool,
    pub no_context_files: bool,
    pub list_models: Option<ListModels>,
    pub offline: bool,
    pub verbose: bool,
    pub project_trust_override: Option<bool>,
    pub messages: Vec<String>,
    pub file_args: Vec<String>,
    /// Unknown flags (potentially extension flags) in first-insertion order,
    /// like the upstream `Map<string, boolean | string>` (args.ts:53).
    pub unknown_flags: Vec<(String, UnknownFlagValue)>,
    pub diagnostics: Vec<Diagnostic>,
}

impl Args {
    /// `Map.set` semantics: overwrite in place when the flag repeats.
    fn set_unknown_flag(&mut self, name: &str, value: UnknownFlagValue) {
        if let Some(entry) = self.unknown_flags.iter_mut().find(|(n, _)| n == name) {
            entry.1 = value;
        } else {
            self.unknown_flags.push((name.to_owned(), value));
        }
    }

    /// Lookup helper mirroring `Map.get`.
    pub fn unknown_flag(&self, name: &str) -> Option<&UnknownFlagValue> {
        self.unknown_flags
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v)
    }
}

/// `VALID_THINKING_LEVELS` (args.ts:57).
pub const VALID_THINKING_LEVELS: [&str; 7] =
    ["off", "minimal", "low", "medium", "high", "xhigh", "max"];

/// `isValidThinkingLevel` (args.ts:59-61): parse, `None` when invalid.
pub fn parse_thinking_level(level: &str) -> Option<ThinkingLevel> {
    match level {
        "off" => Some(ThinkingLevel::Off),
        "minimal" => Some(ThinkingLevel::Minimal),
        "low" => Some(ThinkingLevel::Low),
        "medium" => Some(ThinkingLevel::Medium),
        "high" => Some(ThinkingLevel::High),
        "xhigh" => Some(ThinkingLevel::Xhigh),
        "max" => Some(ThinkingLevel::Max),
        _ => None,
    }
}

/// `parseArgs` (args.ts:63-210).
pub fn parse_args(args: &[String]) -> Args {
    let mut result = Args::default();

    let mut i = 0;
    while i < args.len() {
        let arg = &args[i];

        if arg == "--help" || arg == "-h" {
            result.help = true;
        } else if arg == "--version" || arg == "-v" {
            result.version = true;
        } else if arg == "--mode" && i + 1 < args.len() {
            i += 1;
            let mode = &args[i];
            if mode == "text" || mode == "json" || mode == "rpc" {
                result.mode = Some(match mode.as_str() {
                    "text" => Mode::Text,
                    "json" => Mode::Json,
                    _ => Mode::Rpc,
                });
            }
        } else if arg == "--continue" || arg == "-c" {
            result.continue_ = true;
        } else if arg == "--resume" || arg == "-r" {
            result.resume = true;
        } else if arg == "--provider" && i + 1 < args.len() {
            i += 1;
            result.provider = Some(args[i].clone());
        } else if arg == "--model" && i + 1 < args.len() {
            i += 1;
            result.model = Some(args[i].clone());
        } else if arg == "--api-key" && i + 1 < args.len() {
            i += 1;
            result.api_key = Some(args[i].clone());
        } else if arg == "--system-prompt" && i + 1 < args.len() {
            i += 1;
            result.system_prompt = Some(args[i].clone());
        } else if arg == "--append-system-prompt" && i + 1 < args.len() {
            i += 1;
            result
                .append_system_prompt
                .get_or_insert_with(Vec::new)
                .push(args[i].clone());
        } else if arg == "--name" || arg == "-n" {
            if i + 1 < args.len() {
                i += 1;
                result.name = Some(args[i].clone());
            } else {
                result
                    .diagnostics
                    .push(Diagnostic::error("--name requires a value"));
            }
        } else if arg == "--no-session" {
            result.no_session = true;
        } else if arg == "--session" && i + 1 < args.len() {
            i += 1;
            result.session = Some(args[i].clone());
        } else if arg == "--session-id" && i + 1 < args.len() {
            i += 1;
            result.session_id = Some(args[i].clone());
        } else if arg == "--fork" && i + 1 < args.len() {
            i += 1;
            result.fork = Some(args[i].clone());
        } else if arg == "--session-dir" && i + 1 < args.len() {
            i += 1;
            result.session_dir = Some(args[i].clone());
        } else if arg == "--models" && i + 1 < args.len() {
            i += 1;
            result.models = Some(args[i].split(',').map(|s| s.trim().to_owned()).collect());
        } else if arg == "--no-tools" || arg == "-nt" {
            result.no_tools = true;
        } else if arg == "--no-builtin-tools" || arg == "-nbt" {
            result.no_builtin_tools = true;
        } else if (arg == "--tools" || arg == "-t") && i + 1 < args.len() {
            i += 1;
            result.tools = Some(
                args[i]
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect(),
            );
        } else if (arg == "--exclude-tools" || arg == "-xt") && i + 1 < args.len() {
            i += 1;
            result.exclude_tools = Some(
                args[i]
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|name| !name.is_empty())
                    .collect(),
            );
        } else if arg == "--thinking" && i + 1 < args.len() {
            i += 1;
            let level = &args[i];
            if let Some(level) = parse_thinking_level(level) {
                result.thinking = Some(level);
            } else {
                result.diagnostics.push(Diagnostic::warning(format!(
                    "Invalid thinking level \"{level}\". Valid values: {}",
                    VALID_THINKING_LEVELS.join(", ")
                )));
            }
        } else if arg == "--print" || arg == "-p" {
            result.print = true;
            if let Some(next) = args.get(i + 1) {
                if !next.starts_with('@') && (!next.starts_with('-') || next.starts_with("---")) {
                    result.messages.push(next.clone());
                    i += 1;
                }
            }
        } else if arg == "--export" && i + 1 < args.len() {
            i += 1;
            result.export = Some(args[i].clone());
        } else if (arg == "--extension" || arg == "-e") && i + 1 < args.len() {
            i += 1;
            result
                .extensions
                .get_or_insert_with(Vec::new)
                .push(args[i].clone());
        } else if arg == "--no-extensions" || arg == "-ne" {
            result.no_extensions = true;
        } else if arg == "--skill" && i + 1 < args.len() {
            i += 1;
            result
                .skills
                .get_or_insert_with(Vec::new)
                .push(args[i].clone());
        } else if arg == "--prompt-template" && i + 1 < args.len() {
            i += 1;
            result
                .prompt_templates
                .get_or_insert_with(Vec::new)
                .push(args[i].clone());
        } else if arg == "--theme" && i + 1 < args.len() {
            i += 1;
            result
                .themes
                .get_or_insert_with(Vec::new)
                .push(args[i].clone());
        } else if arg == "--no-skills" || arg == "-ns" {
            result.no_skills = true;
        } else if arg == "--no-prompt-templates" || arg == "-np" {
            result.no_prompt_templates = true;
        } else if arg == "--no-themes" {
            result.no_themes = true;
        } else if arg == "--no-context-files" || arg == "-nc" {
            result.no_context_files = true;
        } else if arg == "--list-models" {
            // Check if next arg is a search pattern (not a flag or file arg).
            if i + 1 < args.len() && !args[i + 1].starts_with('-') && !args[i + 1].starts_with('@')
            {
                i += 1;
                result.list_models = Some(ListModels::Search(args[i].clone()));
            } else {
                result.list_models = Some(ListModels::All);
            }
        } else if arg == "--verbose" {
            result.verbose = true;
        } else if arg == "--approve" || arg == "-a" {
            result.project_trust_override = Some(true);
        } else if arg == "--no-approve" || arg == "-na" {
            result.project_trust_override = Some(false);
        } else if arg == "--offline" {
            result.offline = true;
        } else if let Some(file_arg) = arg.strip_prefix('@') {
            result.file_args.push(file_arg.to_owned());
        } else if let Some(long_flag) = arg.strip_prefix("--") {
            match long_flag.find('=') {
                Some(eq_index) => {
                    result.set_unknown_flag(
                        &long_flag[..eq_index],
                        UnknownFlagValue::String(long_flag[eq_index + 1..].to_owned()),
                    );
                }
                None => {
                    let next = args.get(i + 1);
                    match next {
                        Some(next) if !next.starts_with('-') && !next.starts_with('@') => {
                            result.set_unknown_flag(
                                long_flag,
                                UnknownFlagValue::String(next.clone()),
                            );
                            i += 1;
                        }
                        _ => {
                            result.set_unknown_flag(long_flag, UnknownFlagValue::Boolean(true));
                        }
                    }
                }
            }
        } else if arg.starts_with('-') {
            // Unknown short option (single `-` prefix).
            result
                .diagnostics
                .push(Diagnostic::error(format!("Unknown option: {arg}")));
        } else {
            result.messages.push(arg.clone());
        }

        i += 1;
    }

    result
}

/// `ExtensionFlag` (core/extensions/types.ts) — flags registered by
/// extensions for the dynamic help section. Populated by the extension host
/// (T15); the type lives here because help rendering needs it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExtensionFlag {
    pub name: String,
    /// `"boolean" | "string"`.
    pub flag_type: String,
    pub description: Option<String>,
    pub extension_path: String,
}

fn bold(text: &str, use_ansi: bool) -> String {
    if use_ansi {
        format!("\x1b[1m{text}\x1b[0m")
    } else {
        text.to_owned()
    }
}

/// `printHelp` (args.ts:212-393). Returns the help text; the caller prints
/// it. `use_ansi` mirrors chalk's TTY auto-detection (bold headings only on a
/// terminal).
pub fn print_help(extension_flags: &[ExtensionFlag], use_ansi: bool) -> String {
    let extension_flags_text = if extension_flags.is_empty() {
        String::new()
    } else {
        let lines: Vec<String> = extension_flags
            .iter()
            .map(|flag| {
                let value = if flag.flag_type == "string" {
                    " <value>"
                } else {
                    ""
                };
                let description = flag
                    .description
                    .clone()
                    .unwrap_or_else(|| format!("Registered by {}", flag.extension_path));
                format!(
                    "  --{}{:<width$}{}",
                    flag.name,
                    value,
                    description,
                    width = 30 - flag.name.len().min(28)
                )
            })
            .collect();
        format!(
            "\n{}\n{}\n",
            bold("Extension CLI Flags:", use_ansi),
            lines.join("\n")
        )
    };

    format!(
        r#"{app_bold} - AI coding assistant with read, bash, edit, write tools

{usage_bold}
  {APP_NAME} [options] [@files...] [messages...]

{commands_bold}
  {APP_NAME} install <source> [-l]     Install extension source and add to settings
  {APP_NAME} remove <source> [-l]      Remove extension source from settings
  {APP_NAME} uninstall <source> [-l]   Alias for remove
  {APP_NAME} update [source|self|pi]   Update pi, extensions, or model catalogs
  {APP_NAME} list                      List installed extensions from settings
  {APP_NAME} config [-l]               Open TUI to enable/disable package resources (Tab switches scope)
  {APP_NAME} <command> --help          Show help for install/remove/uninstall/update/list/config

{options_bold}
  --provider <name>              Provider name (default: google)
  --model <pattern>              Model pattern or ID (supports "provider/id" and optional ":<thinking>")
  --api-key <key>                API key (defaults to env vars)
  --system-prompt <text>         System prompt (default: coding assistant prompt)
  --append-system-prompt <text>  Append text or file contents to the system prompt (can be used multiple times)
  --mode <mode>                  Output mode: text (default), json, or rpc
  --print, -p                    Non-interactive mode: process prompt and exit
  --continue, -c                 Continue previous session
  --resume, -r                   Select a session to resume
  --session <path|id>            Use specific session file or partial UUID
  --session-id <id>              Use exact project session ID, creating it if missing
  --fork <path|id>               Fork specific session file or partial UUID into a new session
  --session-dir <dir>            Directory for session storage and lookup
  --no-session                   Don't save session (ephemeral)
  --name, -n <name>              Set session display name
  --models <patterns>            Comma-separated model patterns for Ctrl+P cycling
                                 Supports globs (anthropic/*, *sonnet*) and fuzzy matching
  --no-tools, -nt                Disable all tools by default (built-in and extension)
  --no-builtin-tools, -nbt       Disable built-in tools by default but keep extension/custom tools enabled
  --tools, -t <tools>            Comma-separated allowlist of tool names to enable
                                 Applies to built-in, extension, and custom tools
  --exclude-tools, -xt <tools>   Comma-separated denylist of tool names to disable
                                 Applies to built-in, extension, and custom tools
  --thinking <level>             Set thinking level: off, minimal, low, medium, high, xhigh, max
  --extension, -e <path>         Load an extension file (can be used multiple times)
  --no-extensions, -ne           Disable extension discovery (explicit -e paths still work)
  --skill <path>                 Load a skill file or directory (can be used multiple times)
  --no-skills, -ns               Disable skills discovery and loading
  --prompt-template <path>       Load a prompt template file or directory (can be used multiple times)
  --no-prompt-templates, -np     Disable prompt template discovery and loading
  --theme <path>                 Load a theme file or directory (can be used multiple times)
  --no-themes                    Disable theme discovery and loading
  --no-context-files, -nc        Disable AGENTS.md and CLAUDE.md discovery and loading
  --export <file>                Export session file to HTML and exit
  --list-models [search]         List available models (with optional fuzzy search)
  --verbose                      Force verbose startup (overrides quietStartup setting)
  --approve, -a                  Trust project-local files for this run
  --no-approve, -na              Ignore project-local files for this run
  --offline                      Disable startup network operations (same as PIR_OFFLINE=1)
  --help, -h                     Show this help
  --version, -v                  Show version number

Extensions can register additional flags (e.g., --plan from plan-mode extension).{extension_flags_text}

{examples_bold}
  # Interactive mode
  {APP_NAME}

  # Interactive mode with initial prompt
  {APP_NAME} "List all .ts files in src/"

  # Include files in initial message
  {APP_NAME} @prompt.md @image.png "What color is the sky?"

  # Non-interactive mode (process and exit)
  {APP_NAME} -p "List all .ts files in src/"

  # Multiple messages (interactive)
  {APP_NAME} "Read package.json" "What dependencies do we have?"

  # Continue previous session
  {APP_NAME} --continue "What did we discuss?"

  # Start a named session
  {APP_NAME} --name "Refactor auth module"

  # Use different model
  {APP_NAME} --provider openai --model gpt-4o-mini "Help me refactor this code"

  # Use model with provider prefix (no --provider needed)
  {APP_NAME} --model openai/gpt-4o "Help me refactor this code"

  # Use model with thinking level shorthand
  {APP_NAME} --model sonnet:high "Solve this complex problem"

  # Limit model cycling to specific models
  {APP_NAME} --models claude-sonnet,claude-haiku,gpt-4o

  # Limit to a specific provider with glob pattern
  {APP_NAME} --models "github-copilot/*"

  # Cycle models with fixed thinking levels
  {APP_NAME} --models sonnet:high,haiku:low

  # Start with a specific thinking level
  {APP_NAME} --thinking high "Solve this complex problem"

  # Read-only mode (no file modifications possible)
  {APP_NAME} --tools read,grep,find,ls -p "Review the code in src/"

  # Disable one tool while keeping the rest available
  {APP_NAME} --exclude-tools ask_question

  # Export a session file to HTML
  {APP_NAME} --export ~/{CONFIG_DIR_NAME}/agent/sessions/--path--/session.jsonl
  {APP_NAME} --export session.jsonl output.html

{env_bold}
  ANTHROPIC_AUTH_TOKEN             - Anthropic bearer auth token
  ANTHROPIC_API_KEY                - Anthropic Claude API key
  ANTHROPIC_OAUTH_TOKEN            - Anthropic OAuth token (alternative to API key)
  ANT_LING_API_KEY                 - Ant Ling API key
  OPENAI_API_KEY                   - OpenAI GPT API key
  AZURE_OPENAI_API_KEY             - Azure OpenAI API key
  AZURE_OPENAI_BASE_URL            - Azure OpenAI/Cognitive Services base URL (e.g. https://{{resource}}.openai.azure.com)
  AZURE_OPENAI_RESOURCE_NAME       - Azure OpenAI resource name (alternative to base URL)
  AZURE_OPENAI_API_VERSION         - Azure OpenAI API version (default: v1)
  AZURE_OPENAI_DEPLOYMENT_NAME_MAP - Azure OpenAI model=deployment map (comma-separated)
  DEEPSEEK_API_KEY                 - DeepSeek API key
  NVIDIA_API_KEY                   - NVIDIA NIM API key
  GEMINI_API_KEY                   - Google Gemini API key
  GROQ_API_KEY                     - Groq API key
  CEREBRAS_API_KEY                 - Cerebras API key
  XAI_API_KEY                      - xAI Grok API key
  FIREWORKS_API_KEY                - Fireworks API key
  TOGETHER_API_KEY                 - Together AI API key
  OPENROUTER_API_KEY               - OpenRouter API key
  AI_GATEWAY_API_KEY               - Vercel AI Gateway API key
  ZAI_API_KEY                      - ZAI Coding Plan API key (Global)
  ZAI_CODING_CN_API_KEY            - ZAI Coding Plan API key (China)
  MISTRAL_API_KEY                  - Mistral API key
  MINIMAX_API_KEY                  - MiniMax API key
  MOONSHOT_API_KEY                 - Moonshot AI API key
  OPENCODE_API_KEY                 - OpenCode Zen/OpenCode Go API key
  KIMI_API_KEY                     - Kimi For Coding API key
  CLOUDFLARE_API_KEY               - Cloudflare API token (Workers AI and AI Gateway)
  CLOUDFLARE_ACCOUNT_ID            - Cloudflare account id (required for both)
  CLOUDFLARE_GATEWAY_ID            - Cloudflare AI Gateway slug (required for AI Gateway)
  QWEN_TOKEN_PLAN_API_KEY          - Qwen Token Plan API key (international region)
  QWEN_TOKEN_PLAN_CN_API_KEY       - Qwen Token Plan API key (China region)
  XIAOMI_API_KEY                   - Xiaomi MiMo API key (api.xiaomimimo.com billing)
  XIAOMI_TOKEN_PLAN_CN_API_KEY     - Xiaomi MiMo Token Plan API key (China region)
  XIAOMI_TOKEN_PLAN_AMS_API_KEY    - Xiaomi MiMo Token Plan API key (Amsterdam region)
  XIAOMI_TOKEN_PLAN_SGP_API_KEY    - Xiaomi MiMo Token Plan API key (Singapore region)
  AWS_PROFILE                      - AWS profile for Amazon Bedrock
  AWS_ACCESS_KEY_ID                - AWS access key for Amazon Bedrock
  AWS_SECRET_ACCESS_KEY            - AWS secret key for Amazon Bedrock
  AWS_BEARER_TOKEN_BEDROCK         - Bedrock API key (bearer token)
  AWS_REGION                       - AWS region for Amazon Bedrock (e.g., us-east-1)
  {ENV_AGENT_DIR:<32} - Config directory (default: ~/{CONFIG_DIR_NAME}/agent)
  {ENV_SESSION_DIR:<32} - Session storage directory (overridden by --session-dir)
  PIR_PACKAGE_DIR                  - Override package directory (for Nix/Guix store paths)
  PIR_OFFLINE                      - Disable startup network operations when set to 1/true/yes
  PIR_TELEMETRY                    - Override install telemetry when set to 1/true/yes or 0/false/no
  PIR_SHARE_VIEWER_URL             - Base URL for /share command (default: https://pi.dev/session/)

{tools_bold}
  read   - Read file contents
  bash   - Execute bash commands
  edit   - Edit files with find/replace
  write  - Write files (creates/overwrites)
  grep   - Search file contents (read-only, off by default)
  find   - Find files by glob pattern (read-only, off by default)
  ls     - List directory contents (read-only, off by default)
"#,
        app_bold = bold(APP_NAME, use_ansi),
        usage_bold = bold("Usage:", use_ansi),
        commands_bold = bold("Commands:", use_ansi),
        options_bold = bold("Options:", use_ansi),
        examples_bold = bold("Examples:", use_ansi),
        env_bold = bold("Environment Variables:", use_ansi),
        tools_bold = bold("Built-in Tool Names:", use_ansi),
        extension_flags_text = extension_flags_text,
    )
}

#[cfg(test)]
mod tests {
    //! Port of `packages/coding-agent/test/args.test.ts`.

    use super::*;

    fn args(input: &[&str]) -> Args {
        parse_args(&input.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    // --version flag

    #[test]
    fn test_parses_version_flag() {
        assert!(args(&["--version"]).version);
    }

    #[test]
    fn test_parses_v_shorthand() {
        assert!(args(&["-v"]).version);
    }

    #[test]
    fn test_version_takes_precedence_over_other_args() {
        let result = args(&["--version", "--help", "some message"]);
        assert!(result.version);
        assert!(result.help);
        assert!(result.messages.contains(&"some message".to_owned()));
    }

    // --help flag

    #[test]
    fn test_parses_help_flag() {
        assert!(args(&["--help"]).help);
    }

    #[test]
    fn test_parses_h_shorthand() {
        assert!(args(&["-h"]).help);
    }

    // --print flag

    #[test]
    fn test_parses_print_flag() {
        assert!(args(&["--print"]).print);
    }

    #[test]
    fn test_parses_p_shorthand() {
        assert!(args(&["-p"]).print);
    }

    #[test]
    fn test_parses_prompt_after_p_even_when_it_starts_with_yaml_frontmatter() {
        let prompt = "---\ntitle: hello\n---\nSay hi.";
        let result = args(&["-p", prompt]);
        assert!(result.print);
        assert_eq!(result.messages, vec![prompt.to_owned()]);
        assert!(result.unknown_flags.is_empty());
    }

    #[test]
    fn test_does_not_consume_options_after_p_as_prompts() {
        let result = args(&["-p", "--provider", "openai", "Say hi."]);
        assert!(result.print);
        assert_eq!(result.provider.as_deref(), Some("openai"));
        assert_eq!(result.messages, vec!["Say hi.".to_owned()]);
    }

    #[test]
    fn test_p_swallows_triple_dash_value() {
        // `---foo` is swallowed; `-foo` / `--foo` are not (args.ts:143).
        let result = args(&["-p", "---foo"]);
        assert_eq!(result.messages, vec!["---foo".to_owned()]);

        let result = args(&["-p", "-foo"]);
        assert!(result.messages.is_empty());
        assert_eq!(
            result.diagnostics,
            vec![Diagnostic::error("Unknown option: -foo")]
        );

        let result = args(&["-p", "--foo"]);
        assert!(result.messages.is_empty());
        assert_eq!(
            result.unknown_flag("foo"),
            Some(&UnknownFlagValue::Boolean(true))
        );
    }

    // --continue flag

    #[test]
    fn test_parses_continue_flag() {
        assert!(args(&["--continue"]).continue_);
    }

    #[test]
    fn test_parses_c_shorthand() {
        assert!(args(&["-c"]).continue_);
    }

    // --resume flag

    #[test]
    fn test_parses_resume_flag() {
        assert!(args(&["--resume"]).resume);
    }

    #[test]
    fn test_parses_r_shorthand() {
        assert!(args(&["-r"]).resume);
    }

    // flags with values

    #[test]
    fn test_parses_provider() {
        assert_eq!(
            args(&["--provider", "openai"]).provider.as_deref(),
            Some("openai")
        );
    }

    #[test]
    fn test_parses_model() {
        assert_eq!(
            args(&["--model", "gpt-4o"]).model.as_deref(),
            Some("gpt-4o")
        );
    }

    #[test]
    fn test_parses_api_key() {
        assert_eq!(
            args(&["--api-key", "sk-test-key"]).api_key.as_deref(),
            Some("sk-test-key")
        );
    }

    #[test]
    fn test_parses_system_prompt() {
        assert_eq!(
            args(&["--system-prompt", "You are a helpful assistant"])
                .system_prompt
                .as_deref(),
            Some("You are a helpful assistant")
        );
    }

    #[test]
    fn test_parses_append_system_prompt() {
        assert_eq!(
            args(&["--append-system-prompt", "Additional context"]).append_system_prompt,
            Some(vec!["Additional context".to_owned()])
        );
    }

    #[test]
    fn test_parses_multiple_append_system_prompt_flags() {
        assert_eq!(
            args(&[
                "--append-system-prompt",
                "Context A",
                "--append-system-prompt",
                "Context B"
            ])
            .append_system_prompt,
            Some(vec!["Context A".to_owned(), "Context B".to_owned()])
        );
    }

    #[test]
    fn test_parses_mode() {
        assert_eq!(args(&["--mode", "json"]).mode, Some(Mode::Json));
    }

    #[test]
    fn test_parses_mode_rpc() {
        assert_eq!(args(&["--mode", "rpc"]).mode, Some(Mode::Rpc));
    }

    #[test]
    fn test_parses_session() {
        assert_eq!(
            args(&["--session", "/path/to/session.jsonl"])
                .session
                .as_deref(),
            Some("/path/to/session.jsonl")
        );
    }

    #[test]
    fn test_parses_session_id() {
        assert_eq!(
            args(&["--session-id", "orchestrated-session"])
                .session_id
                .as_deref(),
            Some("orchestrated-session")
        );
    }

    #[test]
    fn test_parses_fork() {
        let result = args(&["--fork", "1234abcd"]);
        assert_eq!(result.fork.as_deref(), Some("1234abcd"));
        assert!(result.messages.is_empty());
    }

    #[test]
    fn test_parses_export() {
        assert_eq!(
            args(&["--export", "session.jsonl"]).export.as_deref(),
            Some("session.jsonl")
        );
    }

    #[test]
    fn test_parses_thinking() {
        assert_eq!(
            args(&["--thinking", "high"]).thinking,
            Some(ThinkingLevel::High)
        );
    }

    #[test]
    fn test_invalid_thinking_level_is_warning_only() {
        let result = args(&["--thinking", "bogus"]);
        assert_eq!(result.thinking, None);
        assert_eq!(
            result.diagnostics,
            vec![Diagnostic::warning(
                "Invalid thinking level \"bogus\". Valid values: off, minimal, low, medium, high, xhigh, max"
            )]
        );
    }

    #[test]
    fn test_parses_models_as_comma_separated_list() {
        assert_eq!(
            args(&["--models", "gpt-4o,claude-sonnet,gemini-pro"]).models,
            Some(vec![
                "gpt-4o".to_owned(),
                "claude-sonnet".to_owned(),
                "gemini-pro".to_owned()
            ])
        );
    }

    // --name flag

    #[test]
    fn test_parses_name_flag_with_value() {
        assert_eq!(
            args(&["--name", "my-session"]).name.as_deref(),
            Some("my-session")
        );
    }

    #[test]
    fn test_parses_n_shorthand() {
        assert_eq!(
            args(&["-n", "quick-session"]).name.as_deref(),
            Some("quick-session")
        );
    }

    #[test]
    fn test_preserves_empty_values_for_main_validation() {
        assert_eq!(args(&["--name", ""]).name.as_deref(), Some(""));
    }

    #[test]
    fn test_reports_missing_value() {
        assert_eq!(
            args(&["--name"]).diagnostics,
            vec![Diagnostic::error("--name requires a value")]
        );
    }

    #[test]
    fn test_name_works_alongside_other_flags() {
        let result = args(&[
            "--name",
            "named-run",
            "--print",
            "--model",
            "gpt-4o",
            "hello",
        ]);
        assert_eq!(result.name.as_deref(), Some("named-run"));
        assert!(result.print);
        assert_eq!(result.model.as_deref(), Some("gpt-4o"));
        assert_eq!(result.messages, vec!["hello".to_owned()]);
    }

    // --no-session flag

    #[test]
    fn test_parses_no_session_flag() {
        assert!(args(&["--no-session"]).no_session);
    }

    // --extension flag

    #[test]
    fn test_parses_single_extension() {
        assert_eq!(
            args(&["--extension", "./my-extension.ts"]).extensions,
            Some(vec!["./my-extension.ts".to_owned()])
        );
    }

    #[test]
    fn test_parses_e_shorthand() {
        assert_eq!(
            args(&["-e", "./my-extension.ts"]).extensions,
            Some(vec!["./my-extension.ts".to_owned()])
        );
    }

    #[test]
    fn test_parses_multiple_extension_flags() {
        assert_eq!(
            args(&["--extension", "./ext1.ts", "-e", "./ext2.ts"]).extensions,
            Some(vec!["./ext1.ts".to_owned(), "./ext2.ts".to_owned()])
        );
    }

    // --no-extensions flag

    #[test]
    fn test_parses_no_extensions_flag() {
        assert!(args(&["--no-extensions"]).no_extensions);
    }

    #[test]
    fn test_parses_no_extensions_with_explicit_e_flags() {
        let result = args(&["--no-extensions", "-e", "foo.ts", "-e", "bar.ts"]);
        assert!(result.no_extensions);
        assert_eq!(
            result.extensions,
            Some(vec!["foo.ts".to_owned(), "bar.ts".to_owned()])
        );
    }

    // --skill flag

    #[test]
    fn test_parses_single_skill() {
        assert_eq!(
            args(&["--skill", "./skill-dir"]).skills,
            Some(vec!["./skill-dir".to_owned()])
        );
    }

    #[test]
    fn test_parses_multiple_skill_flags() {
        assert_eq!(
            args(&["--skill", "./skill-a", "--skill", "./skill-b"]).skills,
            Some(vec!["./skill-a".to_owned(), "./skill-b".to_owned()])
        );
    }

    // --prompt-template flag

    #[test]
    fn test_parses_single_prompt_template() {
        assert_eq!(
            args(&["--prompt-template", "./prompts"]).prompt_templates,
            Some(vec!["./prompts".to_owned()])
        );
    }

    #[test]
    fn test_parses_multiple_prompt_template_flags() {
        assert_eq!(
            args(&["--prompt-template", "./one", "--prompt-template", "./two"]).prompt_templates,
            Some(vec!["./one".to_owned(), "./two".to_owned()])
        );
    }

    // --theme flag

    #[test]
    fn test_parses_single_theme() {
        assert_eq!(
            args(&["--theme", "./theme.json"]).themes,
            Some(vec!["./theme.json".to_owned()])
        );
    }

    #[test]
    fn test_parses_multiple_theme_flags() {
        assert_eq!(
            args(&["--theme", "./dark.json", "--theme", "./light.json"]).themes,
            Some(vec!["./dark.json".to_owned(), "./light.json".to_owned()])
        );
    }

    // --no-skills flag

    #[test]
    fn test_parses_no_skills_flag() {
        assert!(args(&["--no-skills"]).no_skills);
    }

    // --no-prompt-templates flag

    #[test]
    fn test_parses_no_prompt_templates_flag() {
        assert!(args(&["--no-prompt-templates"]).no_prompt_templates);
    }

    // --no-themes flag

    #[test]
    fn test_parses_no_themes_flag() {
        assert!(args(&["--no-themes"]).no_themes);
    }

    // --no-context-files flag

    #[test]
    fn test_parses_no_context_files_flag() {
        assert!(args(&["--no-context-files"]).no_context_files);
    }

    #[test]
    fn test_parses_nc_shorthand() {
        assert!(args(&["-nc"]).no_context_files);
    }

    // project approval flags

    #[test]
    fn test_parses_approve() {
        assert_eq!(args(&["--approve"]).project_trust_override, Some(true));
    }

    #[test]
    fn test_parses_a_shorthand() {
        assert_eq!(args(&["-a"]).project_trust_override, Some(true));
    }

    #[test]
    fn test_parses_no_approve() {
        assert_eq!(args(&["--no-approve"]).project_trust_override, Some(false));
    }

    #[test]
    fn test_parses_na_shorthand() {
        assert_eq!(args(&["-na"]).project_trust_override, Some(false));
    }

    // --verbose flag

    #[test]
    fn test_parses_verbose_flag() {
        assert!(args(&["--verbose"]).verbose);
    }

    // --offline flag

    #[test]
    fn test_parses_offline_flag() {
        assert!(args(&["--offline"]).offline);
    }

    // tool flags

    #[test]
    fn test_parses_no_tools_flag() {
        assert!(args(&["--no-tools"]).no_tools);
    }

    #[test]
    fn test_parses_nt_shorthand() {
        assert!(args(&["-nt"]).no_tools);
    }

    #[test]
    fn test_parses_no_builtin_tools_flag() {
        assert!(args(&["--no-builtin-tools"]).no_builtin_tools);
    }

    #[test]
    fn test_parses_nbt_shorthand() {
        assert!(args(&["-nbt"]).no_builtin_tools);
    }

    #[test]
    fn test_parses_tools_flag() {
        assert_eq!(
            args(&["--tools", "read,bash"]).tools,
            Some(vec!["read".to_owned(), "bash".to_owned()])
        );
    }

    #[test]
    fn test_parses_t_shorthand() {
        assert_eq!(
            args(&["-t", "read,bash"]).tools,
            Some(vec!["read".to_owned(), "bash".to_owned()])
        );
    }

    #[test]
    fn test_parses_exclude_tools_flag() {
        assert_eq!(
            args(&["--exclude-tools", "read,bash"]).exclude_tools,
            Some(vec!["read".to_owned(), "bash".to_owned()])
        );
    }

    #[test]
    fn test_parses_xt_shorthand() {
        assert_eq!(
            args(&["-xt", "read,bash"]).exclude_tools,
            Some(vec!["read".to_owned(), "bash".to_owned()])
        );
    }

    #[test]
    fn test_parses_no_tools_with_explicit_tools_flags() {
        let result = args(&["--no-tools", "--tools", "read,bash"]);
        assert!(result.no_tools);
        assert_eq!(
            result.tools,
            Some(vec!["read".to_owned(), "bash".to_owned()])
        );
    }

    #[test]
    fn test_parses_no_builtin_tools_with_explicit_tools_flags() {
        let result = args(&["--no-builtin-tools", "--tools", "read,bash"]);
        assert!(result.no_builtin_tools);
        assert_eq!(
            result.tools,
            Some(vec!["read".to_owned(), "bash".to_owned()])
        );
    }

    // messages and file args

    #[test]
    fn test_parses_plain_text_messages() {
        assert_eq!(
            args(&["hello", "world"]).messages,
            vec!["hello".to_owned(), "world".to_owned()]
        );
    }

    #[test]
    fn test_parses_file_arguments() {
        assert_eq!(
            args(&["@README.md", "@src/main.ts"]).file_args,
            vec!["README.md".to_owned(), "src/main.ts".to_owned()]
        );
    }

    #[test]
    fn test_parses_mixed_messages_and_file_args() {
        let result = args(&["@file.txt", "explain this", "@image.png"]);
        assert_eq!(
            result.file_args,
            vec!["file.txt".to_owned(), "image.png".to_owned()]
        );
        assert_eq!(result.messages, vec!["explain this".to_owned()]);
    }

    #[test]
    fn test_captures_unknown_long_flags_with_string_values() {
        let result = args(&["--unknown-flag", "message"]);
        assert!(result.messages.is_empty());
        assert_eq!(
            result.unknown_flag("unknown-flag"),
            Some(&UnknownFlagValue::String("message".to_owned()))
        );
    }

    #[test]
    fn test_captures_unknown_boolean_long_flags() {
        let result = args(&["--unknown-flag"]);
        assert_eq!(
            result.unknown_flag("unknown-flag"),
            Some(&UnknownFlagValue::Boolean(true))
        );
    }

    #[test]
    fn test_captures_unknown_long_flags_with_equals_syntax() {
        let result = args(&["--unknown-flag=value"]);
        assert_eq!(
            result.unknown_flag("unknown-flag"),
            Some(&UnknownFlagValue::String("value".to_owned()))
        );
    }

    #[test]
    fn test_unknown_short_option_is_error_diagnostic() {
        let result = args(&["-x"]);
        assert_eq!(
            result.diagnostics,
            vec![Diagnostic::error("Unknown option: -x")]
        );
    }

    // complex combinations

    #[test]
    fn test_parses_multiple_flags_together() {
        let result = args(&[
            "--provider",
            "anthropic",
            "--model",
            "claude-sonnet",
            "--print",
            "--thinking",
            "high",
            "@prompt.md",
            "Do the task",
        ]);
        assert_eq!(result.provider.as_deref(), Some("anthropic"));
        assert_eq!(result.model.as_deref(), Some("claude-sonnet"));
        assert!(result.print);
        assert_eq!(result.thinking, Some(ThinkingLevel::High));
        assert_eq!(result.file_args, vec!["prompt.md".to_owned()]);
        assert_eq!(result.messages, vec!["Do the task".to_owned()]);
    }
}
