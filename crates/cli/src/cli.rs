//! Command-line surface: the clap types. Parsing only — every variant is
//! dispatched to a handler in `main`.

use clap::{Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "agentctl",
    version,
    about = "Console client for agentd. Speaks WebSocket to the daemon."
)]
pub(crate) struct Cli {
    /// Daemon base URL.
    #[arg(
        short = 'u',
        long,
        env = "AGENTD_URL",
        default_value = "http://127.0.0.1:7777"
    )]
    pub(crate) url: String,

    /// Connect timeout (ms).
    #[arg(long, default_value_t = 30_000)]
    pub(crate) timeout: u64,

    #[command(subcommand)]
    pub(crate) cmd: Cmd,
}

#[derive(Subcommand, Debug)]
pub(crate) enum Cmd {
    /// Check daemon health (HTTP probe).
    Health,
    /// List registered actions.
    Tools,
    /// Invoke an action.
    Call {
        action: String,
        #[arg(short = 'j', long)]
        json: Option<String>,
        #[arg(short = 'd', long = "data", value_name = "KEY=VAL")]
        data: Vec<String>,
        #[arg(short = 'r', long)]
        result_only: bool,
        #[arg(long)]
        compact: bool,
    },
    /// Runner operations.
    #[command(visible_alias = "runners")]
    Runner {
        #[command(subcommand)]
        cmd: RunnerCmd,
    },
    /// Skill operations.
    #[command(name = "skill", visible_alias = "skills")]
    Skills {
        #[command(subcommand)]
        cmd: SkillsCmd,
    },
    /// Service operations.
    #[command(name = "service", visible_alias = "services", alias = "svc")]
    Services {
        #[command(subcommand)]
        cmd: ServicesCmd,
    },
    /// Grant management over the privileged control plane.
    Grants {
        #[command(subcommand)]
        cmd: GrantsCmd,
    },
    /// Manage installed packages (~/.local/share/agentd/packages).
    #[command(name = "package", visible_alias = "packages", alias = "pkg")]
    Packages {
        #[command(subcommand)]
        cmd: PkgCmd,
    },
    /// Manage provider API keys in the OS keyring. Changes are visible to a
    /// running daemon immediately — providers read the keyring at call time.
    #[command(name = "secret", visible_alias = "secrets")]
    Secret {
        #[command(subcommand)]
        cmd: SecretCmd,
    },
    /// Generate lua-language-server type stubs into a project's `.luals/`.
    Types {
        /// Project directory (the folder holding `init.lua`). Defaults to the
        /// current directory.
        dir: Option<std::path::PathBuf>,
    },
    /// Tail the JSONL trace file.
    Trace {
        #[arg(long)]
        file: Option<std::path::PathBuf>,
        #[arg(short = 'f', long)]
        follow: bool,
        #[arg(short = 'n', long, default_value_t = 20)]
        lines: usize,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SecretCmd {
    /// Store a secret. Omit `value` to read it from stdin (keeps the key out
    /// of your shell history): `echo "$KEY" | agentctl secret set my_key`.
    Set { name: String, value: Option<String> },
    /// Remove a secret from the keyring.
    #[command(alias = "rm")]
    Unset { name: String },
    /// Show a half-obfuscated preview of a secret (never the full value).
    Peek { name: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum RunnerCmd {
    Ls,
    Inspect {
        name: String,
    },
    Run {
        name: String,
        prompt: String,
        #[arg(long)]
        text_only: bool,
    },
}

#[derive(Subcommand, Debug)]
pub(crate) enum SkillsCmd {
    Ls,
    Inspect { name: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum PkgCmd {
    /// List installed packages and whether an update is available.
    Ls,
    /// Install a package from a git URL.
    Install {
        url: String,
        #[arg(long)]
        r#ref: Option<String>,
    },
    /// Re-pull and re-pin an installed package.
    Update { name: String },
    /// Remove an installed package.
    #[command(alias = "rm")]
    Remove { name: String },
}

#[derive(Subcommand, Debug)]
pub(crate) enum ServicesCmd {
    Ls,
}

#[derive(Subcommand, Debug)]
pub(crate) enum GrantsCmd {
    /// Listen on the control plane for permission-approval requests and
    /// answer them interactively (allow once / forever / deny).
    Listen,
}
