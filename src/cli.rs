//! The single clap-derived front door for the whole CLI.
//!
//! Every verb — the human surface (`up`, `down`) and the
//! machine-facing plumbing (steer verbs + worker self-callbacks) — is
//! declared here as typed args, all at the top level. clap owns parsing, so we
//! get, uniformly and for free across every verb:
//!   • `--help`/`-h` on every subcommand (and it is NON-destructive: `playbook
//!     write --help` prints help instead of writing the literal text `--help`,
//!     which is the accident this migration closes);
//!   • rejection of unknown/mistyped flags (exit 1) instead of silently writing
//!     or ignoring them;
//!   • the `--` end-of-options convention, so a body that genuinely starts with
//!     `--` is still expressible (`… write -- --literal`).
//!
//! Free-form bodies (goal/sensor/playbook/answer/worker-prompt) are modeled as a
//! variadic positional `Vec<String>`; the `-`/heredoc → stdin convention is
//! resolved AFTER parsing by `executor::resolve_body`. A lone `-` stays a
//! sentinel; `a - b` keeps the dash as content (clap treats a bare `-` as a
//! value, not a flag).
//!
//! NOT modeled here on purpose: the hidden `run --detached-id … -- <cmd>`
//! re-exec path. babysit drives that argv and may pass flags this version does
//! not know; it MUST tolerate unknown flags (forward-compat), which is the
//! opposite of clap's strict rejection. main.rs shortcuts it BEFORE clap parses.

use clap::{Args, Parser, Subcommand};

#[derive(Parser, Debug)]
#[command(
    name = "looop",
    version,
    disable_help_subcommand = true,
    // A bare `looop` is not a command: main prints the manual. We surface our own
    // manual rather than clap's auto help for the no-subcommand case.
    arg_required_else_help = false
)]
pub struct Cli {
    #[command(subcommand)]
    pub cmd: Option<Cmd>,
}

#[derive(Subcommand, Debug)]
pub enum Cmd {
    /// Show the manual.
    Help,
    /// Print the version.
    Version,
    /// Interactive setup: choose the agent runner and write its wiring.
    Init,
    /// Bring the autonomous pulse up.
    Up(UpArgs),
    /// Tear the pulse (and workers) down.
    Down,
    /// looop's own detached reconcile-loop body (spawned by `up`).
    Pulse,
    /// Full world snapshot: goals, sensors, fleet, asks.
    State(StateArgs),
    /// Block until the world changes, then print the new state.
    Wait(WaitArgs),
    /// Just the pending asks.
    Asks(AsksArgs),
    /// Answer a pending ask (durable; `--force` to overwrite).
    Answer(AnswerArgs),
    /// Create/replace or archive a goal.
    Goal(GoalArgs),
    /// Create/replace a sensor script.
    Sensor(SensorArgs),
    /// Rewrite the PLAYBOOK.
    Playbook(PlaybookArgs),
    /// One ad-hoc, REVERSIBLE shell command.
    Run(RunArgs),
    /// Spawn / kill a worker session.
    Worker(WorkerArgs),
    /// Worker self-callback: raise a blocking ask for the human.
    Ask(AskArgs),
    /// Kill a session by id.
    Kill(KillArgs),
    /// Type input into an interactive worker.
    Send(SendArgs),
    /// Capture a worker's current screen.
    Screenshot(ScreenshotArgs),
    /// Atomically claim a named lease.
    Claim(ClaimArgs),
    /// Release a named lease.
    Unclaim(ClaimArgs),
}

#[derive(Args, Debug)]
pub struct UpArgs {
    /// Emit pulse logs as JSON.
    #[arg(long)]
    pub json: bool,
}

/// Shared by every action verb that funnels through `run_action`: a one-line
/// journal note appended (timestamped) to journal.md. Parsed from anywhere on
/// the line — it never leaks into a free-form body.
#[derive(Args, Debug, Default)]
pub struct JournalOpt {
    /// One line: what you did and why (appended, timestamped).
    #[arg(long)]
    pub journal: Option<String>,
}

#[derive(Args, Debug)]
pub struct StateArgs {
    /// Emit JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct WaitArgs {
    /// Emit JSON instead of the human summary.
    #[arg(long)]
    pub json: bool,
    /// Wake on asks/journal moves.
    #[arg(long)]
    pub actionable: bool,
    /// Wake only on a new pending ask.
    #[arg(long)]
    pub only_asks: bool,
}

#[derive(Args, Debug)]
pub struct AsksArgs {
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
}

#[derive(Args, Debug)]
pub struct AnswerArgs {
    /// The ask id to answer.
    pub ask_id: String,
    /// The answer text. Omit or pass `-` to read stdin/heredoc.
    pub body: Vec<String>,
    /// Overwrite an already-given answer.
    #[arg(long)]
    pub force: bool,
}

#[derive(Args, Debug)]
pub struct GoalArgs {
    #[command(subcommand)]
    pub op: GoalOp,
}

#[derive(Subcommand, Debug)]
pub enum GoalOp {
    /// Create or replace a goal. Omit body or pass `-` to read stdin/heredoc.
    Write {
        id: String,
        body: Vec<String>,
        #[command(flatten)]
        journal: JournalOpt,
    },
    /// Move goals/<id>.md into archive/.
    Archive {
        id: String,
        #[command(flatten)]
        journal: JournalOpt,
    },
}

#[derive(Args, Debug)]
pub struct SensorArgs {
    #[command(subcommand)]
    pub op: SensorOp,
}

#[derive(Subcommand, Debug)]
pub enum SensorOp {
    /// Create or replace a sensor. Omit script or pass `-` to read stdin/heredoc.
    Write {
        name: String,
        script: Vec<String>,
        #[command(flatten)]
        journal: JournalOpt,
    },
}

#[derive(Args, Debug)]
pub struct PlaybookArgs {
    #[command(subcommand)]
    pub op: PlaybookOp,
}

#[derive(Subcommand, Debug)]
pub enum PlaybookOp {
    /// Rewrite the PLAYBOOK. Omit body or pass `-` to read stdin/heredoc.
    Write {
        body: Vec<String>,
        #[command(flatten)]
        journal: JournalOpt,
    },
}

#[derive(Args, Debug)]
pub struct RunArgs {
    /// Why this command is being run (recorded).
    #[arg(long)]
    pub reason: Option<String>,
    #[command(flatten)]
    pub journal: JournalOpt,
    /// The shell command to run. Its OWN flags are passed through verbatim, so
    /// put `--reason`/`--journal` BEFORE the command (or use `--`).
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub cmd: Vec<String>,
}

#[derive(Args, Debug)]
pub struct WorkerArgs {
    #[command(subcommand)]
    pub op: WorkerOp,
}

#[derive(Subcommand, Debug)]
pub enum WorkerOp {
    /// Spawn a worker. Omit prompt or pass `-` to read stdin/heredoc.
    Start {
        id: String,
        prompt: Vec<String>,
        /// Model to launch this worker with (expanded into the
        /// `worker_command` template's `{{model}}` placeholder). Overrides the
        /// optional `worker_model` config default. Ignored (with a warning) if
        /// the template has no `{{model}}` placeholder.
        #[arg(long)]
        model: Option<String>,
        /// Thinking level for this worker (expanded into the `{{thinking}}`
        /// placeholder). Overrides the optional `worker_thinking` config
        /// default. Ignored (with a warning) if the template lacks it.
        #[arg(long)]
        thinking: Option<String>,
        #[command(flatten)]
        journal: JournalOpt,
    },
    /// Kill a worker by id.
    Kill { id: String },
}

#[derive(Args, Debug)]
pub struct AskArgs {
    /// The worker id raising the ask. Defaults to $LOOOP_SESSION_ID.
    pub worker: Option<String>,
    /// What you need to know from the human.
    #[arg(long)]
    pub prompt: String,
    /// A path/reference the human should look at.
    #[arg(long = "ref")]
    pub reference: Option<String>,
    /// Comma-separated choices to offer.
    #[arg(long, value_delimiter = ',')]
    pub options: Vec<String>,
}

#[derive(Args, Debug)]
pub struct KillArgs {
    pub id: String,
}

#[derive(Args, Debug)]
pub struct SendArgs {
    pub id: String,
    /// The text to type. Variadic; put `--no-newline` anywhere.
    pub text: Vec<String>,
    /// Don't send a trailing Enter.
    #[arg(long = "no-newline", short = 'n')]
    pub no_newline: bool,
}

#[derive(Args, Debug)]
pub struct ScreenshotArgs {
    pub id: Option<String>,
    /// Emit ANSI-colored output.
    #[arg(long)]
    pub ansi: bool,
    /// Emit JSON.
    #[arg(long)]
    pub json: bool,
    /// Emit plain text (default).
    #[arg(long)]
    pub plain: bool,
    /// Don't trim trailing blank lines.
    #[arg(long = "no-trim")]
    pub no_trim: bool,
}

#[derive(Args, Debug)]
pub struct ClaimArgs {
    /// The lease name (defined by the goal, e.g. one per repo).
    pub name: String,
    /// Holding session id. Defaults to $LOOOP_SESSION_ID.
    #[arg(long)]
    pub session: Option<String>,
}
