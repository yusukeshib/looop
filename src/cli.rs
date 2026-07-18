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
    #[command(alias = "w")]
    Worker(WorkerArgs),
    /// Worker self-callback: raise a blocking ask for the human.
    Ask(AskArgs),
    /// Queue a steering message INTO a live worker (picked up via `told` /
    /// piggybacked on its next ask answer).
    Tell(TellArgs),
    /// Worker self-callback: print + consume pending steering messages.
    Told(ToldArgs),
    /// Durable time triggers (one-shot / recurring) that wake the loop when due.
    Schedule(ScheduleArgs),
    /// Kill a session by id.
    Kill(KillArgs),
    /// Capture a worker's current screen.
    #[command(alias = "ss")]
    Screenshot(ScreenshotArgs),
    /// Atomically claim a named lease.
    Claim(ClaimArgs),
    /// Release a named lease.
    Unclaim(ClaimArgs),
    /// Output shell integration (completions). E.g. eval "$(looop config zsh)".
    Config(ConfigArgs),
}

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    pub shell: ConfigShell,
}

#[derive(Subcommand, Debug)]
pub enum ConfigShell {
    /// Output Zsh completions + shell integration.
    Zsh,
    /// Output Bash completions + shell integration.
    Bash,
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
    #[command(alias = "w")]
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
    #[command(alias = "w")]
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
    #[command(alias = "w")]
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
        /// Full launch-command override for this one worker, replacing the
        /// config's `worker_command` template wholesale (e.g. a different
        /// runner, model, or flags). Must contain `{{prompt_file}}` — the
        /// worker's brief — exactly like the template.
        #[arg(long)]
        command: Option<String>,
        /// Post-condition: ONE shell command that must exit 0 once the work
        /// is truly done (compose conditions with `&&`). The pulse runs it
        /// ONCE after the worker dies; a non-zero exit marks the worker
        /// verify:fail in sys-sessions — "exit 0 but nothing happened" wakes
        /// the loop as a FAILED worker instead of a clean corpse.
        #[arg(long)]
        verify: Option<String>,
        /// Resume a DETACHED, ANSWERED ask: inject its question, the human's
        /// answer, and the checkpoint reference into the worker's brief, then
        /// archive the ask/answer pair. The value is the ask id.
        #[arg(long)]
        resume: Option<String>,
        #[command(flatten)]
        journal: JournalOpt,
    },
    /// Kill a worker by id.
    Kill { id: String },
    /// List the fleet with health (busy / waiting-ask / stuck / dead).
    #[command(alias = "ls")]
    List {
        /// Emit JSON instead of the table.
        #[arg(long, conflicts_with = "watch")]
        json: bool,
        /// Also show finished/dead workers, not just the live fleet.
        #[arg(long, short = 'a')]
        all: bool,
        /// Re-render every few seconds until Ctrl-C.
        #[arg(long, short = 'w')]
        watch: bool,
        /// Refresh interval for --watch, in seconds.
        #[arg(long, default_value_t = 2, requires = "watch")]
        interval: u64,
    },
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
    /// Don't block: write the ask and return immediately (prints the ask id).
    /// For LONG waits — checkpoint your state to reports/ first, then exit;
    /// when the human answers, looop re-dispatches a fresh worker with the
    /// answer (`worker start --resume <ask_id>`).
    #[arg(long)]
    pub detach: bool,
}

#[derive(Args, Debug)]
pub struct KillArgs {
    pub id: String,
}

#[derive(Args, Debug)]
pub struct TellArgs {
    /// The live worker to steer.
    pub worker: String,
    /// The steering message.
    #[arg(trailing_var_arg = true)]
    pub body: Vec<String>,
}

#[derive(Args, Debug)]
pub struct ToldArgs {
    /// The worker draining its messages. Defaults to $LOOOP_SESSION_ID.
    pub worker: Option<String>,
}

#[derive(Args, Debug)]
pub struct ScheduleArgs {
    #[command(subcommand)]
    pub op: ScheduleOp,
}

#[derive(Subcommand, Debug)]
pub enum ScheduleOp {
    /// Create or replace a schedule. Exactly one of --in / --every.
    #[command(alias = "w")]
    Write {
        name: String,
        /// One-shot: fire once, this many seconds from now.
        #[arg(long = "in", value_name = "SECS")]
        in_s: Option<u64>,
        /// Recurring: fire every N seconds (min 60).
        #[arg(long, value_name = "SECS")]
        every: Option<u64>,
        /// Why this trigger exists (shown to the decider when it fires).
        #[arg(long)]
        note: Option<String>,
        #[command(flatten)]
        journal: JournalOpt,
    },
    /// Remove a schedule.
    Rm {
        name: String,
        #[command(flatten)]
        journal: JournalOpt,
    },
    /// List schedules with their current signal (pending/due/period).
    #[command(alias = "ls")]
    List {
        /// Emit JSON.
        #[arg(long)]
        json: bool,
    },
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
