//! `ctx` — the RecurOS command-line interface.

mod clipboard;
mod doctor;
mod eval;
mod flow;
mod http;

use std::io::{self, IsTerminal, Read};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use chrono::{DateTime, NaiveDate, Utc};
use clap::{Parser, Subcommand, ValueEnum};
use ctx_app::{App, PackOpts, SaveOutcome, VerifyResult, first_line};
use std::collections::{BTreeMap, BTreeSet};

use ctx_branch::Binding;
use ctx_core::{BranchRef, Claim, ClaimDraft, Confidence, Filter, Kind, Status, Store};
use ctx_git::CtxHome;
use ctx_pack::{Projection, UNLIMITED};
use ctx_wire::Agent;

#[derive(Parser)]
#[command(
    name = "ctx",
    version,
    about = "RecurOS: one shared, versioned memory for every AI tool you use",
    long_about = "RecurOS: one shared, versioned memory for every AI tool you use.\n\n\
        Save decisions, constraints and rejected ideas once; every agent (Claude Code, Codex, \
        Cursor, Gemini, claude.ai, ChatGPT, local models) gets a compiled, token-budgeted view.\n\n\
        Start with `ctx init` inside a project.",
    after_help = "Examples:\n  \
        ctx init                                   wire this repo to its context branch\n  \
        ctx save \"Use SQLite, not Postgres\" -k decision -w \"no server to run\"\n  \
        ctx pack                                   print the compiled context\n  \
        ctx pack --for handoff > HANDOFF.md        a handoff doc for a teammate\n  \
        ctx pack | ollama run qwen3                feed a local model"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start a new idea: creates <idea>/research and <idea>/code and makes
    /// research the branch chat surfaces write to.
    New {
        /// Name of the idea / project.
        name: String,
    },
    /// Save, show or list the project's spec (and other long documents).
    #[command(subcommand)]
    Spec(SpecCmd),
    /// Hand an idea to a coding agent: repo + SPEC.md + AGENTS.md + wiring.
    Build {
        /// The idea / project to build.
        name: String,
        /// Directory to create (default: ./<name>).
        dir: Option<PathBuf>,
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
        #[arg(long)]
        no_agents: bool,
    },
    /// Draw the project as a metro map (Mermaid): branches are lines, claims
    /// are stations, colour is the kind.
    Map {
        /// Project (default: the current one).
        project: Option<String>,
        /// Most recent claims shown per branch.
        #[arg(short = 'n', long, default_value_t = 12)]
        per_branch: usize,
        /// Write to a file (e.g. MAP.md) instead of printing.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Wire the current project: .ctx.yaml, AGENTS.md, agent configs. Idempotent.
    Init {
        /// Project name (default: the directory name).
        #[arg(long)]
        project: Option<String>,
        /// Branch this repo's agents use (default: code).
        #[arg(long)]
        branch: Option<String>,
        /// Agents to wire, comma-separated (default: every installed one).
        #[arg(long, value_delimiter = ',')]
        agents: Vec<String>,
        /// Only write .ctx.yaml and AGENTS.md; don't touch agent configs.
        #[arg(long)]
        no_agents: bool,
    },
    /// Record a claim. Instant and offline: no network, no model call.
    Save {
        /// The claim. Use `-` to read from stdin.
        text: Option<String>,
        #[arg(short, long, value_enum, default_value_t = KindArg::Fact)]
        kind: KindArg,
        /// Why it's true, or why it was decided.
        #[arg(short, long)]
        why: Option<String>,
        /// References: paths, repo@sha, URLs. Comma-separated or repeated.
        #[arg(short, long, value_delimiter = ',')]
        refs: Vec<String>,
        /// Topic tags. Comma-separated or repeated.
        #[arg(short, long = "tag", value_delimiter = ',')]
        tags: Vec<String>,
        #[arg(short, long, value_enum, default_value_t = ConfidenceArg::Medium)]
        confidence: ConfidenceArg,
        /// Branch to save to (default: this repo's branch, or `ctx use`).
        #[arg(long)]
        to: Option<String>,
        /// This claim replaces an older one (id or c:cid prefix).
        #[arg(long)]
        supersedes: Option<String>,
        /// Read a ```ctx-claims block (from a chat) off the clipboard.
        #[arg(long, conflicts_with = "text")]
        paste: bool,
        #[arg(long, default_value = "cli", hide = true)]
        src: String,
    },
    /// Full-text search over claims.
    Search {
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        #[arg(short, long)]
        branch: Option<String>,
        #[arg(short, long, value_enum)]
        kind: Vec<KindArg>,
        #[arg(short = 'n', long, default_value_t = 20)]
        limit: usize,
        #[arg(short, long)]
        verbose: bool,
        /// Include claims you have deleted.
        #[arg(short, long)]
        all: bool,
    },
    /// Compile a branch's context into markdown.
    Pack {
        /// Branch (default: this repo's branch, or `ctx use`).
        branch: Option<String>,
        /// Focus the pack on a task.
        #[arg(long)]
        task: Option<String>,
        /// Token budget.
        #[arg(long)]
        budget: Option<u32>,
        /// Shape: agents-md, markdown, dossier, prose/handoff.
        #[arg(long = "for", value_name = "SHAPE")]
        projection: Option<String>,
        /// Copy to the clipboard instead of printing.
        #[arg(long)]
        clip: bool,
        /// Write to a file. For AGENTS.md, only the RecurOS section is replaced.
        #[arg(long)]
        out: Option<PathBuf>,
    },
    /// Set the default branch for chat surfaces and commands outside a repo.
    Use { branch: String },
    /// Where am I: store, branch, counts, pending proposals.
    Status,
    /// Every command, by what you want to do (also in .ctx/commands.md).
    Commands,
    /// Manage context branches.
    #[command(subcommand)]
    Branch(BranchCmd),
    /// Review proposed claims: list them, or accept/reject one.
    Review {
        #[command(subcommand)]
        action: Option<ReviewCmd>,
    },
    /// Show one claim in full, with its history.
    Show { id: String },
    /// Fill an existing project's context from its own code: prints a
    /// prompt to hand to Claude Code, Cursor, Codex or Gemini CLI.
    Onboard {
        /// Copy it to the clipboard instead of printing it.
        #[arg(long)]
        clip: bool,
    },
    /// Every idea you have context for, with its branches and sizes.
    #[command(visible_alias = "ls")]
    List {
        /// Include ideas you have deleted.
        #[arg(short, long)]
        all: bool,
    },
    /// Rename an idea, carrying its claims and documents over.
    Rename {
        /// The idea to rename.
        from: String,
        /// Its new name.
        to: String,
        /// Don't ask for confirmation.
        #[arg(short, long)]
        yes: bool,
    },
    /// Delete a claim (c:xxxx), a branch (idea/branch) or a whole idea.
    /// It disappears everywhere but stays in the history.
    #[command(visible_alias = "delete")]
    Remove {
        /// c:xxxx for one claim, idea/branch for a branch, or an idea name.
        target: String,
        /// Don't ask for confirmation.
        #[arg(short, long)]
        yes: bool,
        /// Fail unless the delete also reaches the cloud (ChatGPT / claude.ai).
        /// Without it, the delete still syncs, but only warns if it can't.
        #[arg(long)]
        cloud: bool,
    },
    /// Retire a claim (a status flip; nothing is ever deleted).
    #[command(hide = true)]
    Archive {
        id: String,
        #[arg(long)]
        reason: Option<String>,
    },
    /// Mark a claim as helpful (up) or harmful (down); feeds pack ranking.
    Rate { id: String, verdict: Verdict },
    /// Find near-duplicate claims worth merging (no model involved).
    Refine {
        #[arg(short, long)]
        branch: Option<String>,
        /// Similarity threshold, 0-1.
        #[arg(long, default_value_t = 0.6)]
        threshold: f64,
    },
    /// Commit, pull and push the store (works offline: then it just commits).
    Sync,
    /// Compare a pack/state root from elsewhere with this machine.
    Verify {
        root: Option<String>,
        #[arg(short, long)]
        branch: Option<String>,
    },
    /// Drop and rebuild the index (.cache/ctx.db) from the log.
    Reindex,
    /// Show recorded claims, oldest first.
    Log {
        #[arg(short, long)]
        branch: Option<String>,
        #[arg(short, long, value_enum)]
        kind: Vec<KindArg>,
        /// Only claims recorded on or after this date (YYYY-MM-DD).
        #[arg(long)]
        since: Option<NaiveDate>,
        /// Show at most this many (the most recent). 0 = all.
        #[arg(short = 'n', long, default_value_t = 50)]
        limit: usize,
        #[arg(short, long)]
        verbose: bool,
        /// Include deleted (archived) claims.
        #[arg(short, long)]
        all: bool,
    },
    /// Score packs against questions with known answers, to tune weights.
    Eval {
        /// Fixture file (default: <store>/refs/eval.yaml).
        #[arg(long)]
        fixtures: Option<PathBuf>,
        /// Ollama model to answer with; without it, checks the pack itself.
        #[arg(long)]
        model: Option<String>,
        /// Budgets to compare, comma-separated.
        #[arg(long, value_delimiter = ',', default_value = "300,700,1500")]
        budget: Vec<u32>,
        #[arg(long, default_value = "http://127.0.0.1:11434")]
        ollama: String,
        /// Write an example fixture file and exit.
        #[arg(long)]
        example: bool,
    },
    /// Diagnose the store, git, this repo's wiring and the daemon.
    Doctor,
    /// Run the MCP server on stdio (agents start this; you don't need to).
    Mcp,
    /// Run the local HTTP API for the browser extension.
    Daemon {
        #[command(subcommand)]
        action: Option<DaemonCmd>,
        #[arg(long, default_value = ctx_daemon::DEFAULT_ADDR)]
        addr: String,
    },
    /// Pair and run the local context node used by remote MCP clients.
    #[command(subcommand)]
    Node(NodeCmd),
    /// Initialise and run a self-hosted RecurOS relay.
    #[command(subcommand)]
    Relay(RelayCmd),
    /// Agent hook entry points (called by agents, not by you).
    #[command(subcommand, hide = true)]
    Hook(HookCmd),
}

#[derive(Subcommand)]
enum SpecCmd {
    /// Save a spec from a file, stdin (-), or the clipboard (--paste). A
    /// ````ctx-spec fenced block is unwrapped; plain markdown is taken as is.
    Save {
        file: Option<PathBuf>,
        #[arg(long, conflicts_with = "file")]
        paste: bool,
        /// Document name.
        #[arg(long, default_value = "spec")]
        name: String,
        #[arg(long)]
        title: Option<String>,
        /// Branch (default: current; specs usually live on <idea>/research).
        #[arg(long)]
        to: Option<String>,
    },
    /// Print a document (default: the spec).
    Show {
        #[arg(default_value = "spec")]
        name: String,
        #[arg(short, long)]
        branch: Option<String>,
    },
    /// List documents visible from a branch.
    Ls {
        #[arg(short, long)]
        branch: Option<String>,
    },
}

#[derive(Subcommand)]
enum BranchCmd {
    /// Create a branch: `ctx branch new gtm --parent research --template gtm`.
    New {
        name: String,
        #[arg(long)]
        parent: Option<String>,
        /// research, code, gtm or writing.
        #[arg(long)]
        template: Option<String>,
        /// Extra visibility, e.g. `code:constraint` (repeatable).
        #[arg(long)]
        inherits: Vec<String>,
    },
    /// List branches with claim counts.
    Ls,
    /// Copy a branch's active claims into another (appends; originals stay).
    Merge {
        src: String,
        #[arg(long)]
        into: String,
    },
    /// Archive a branch and its claims (nothing is deleted).
    Archive { name: String },
}

#[derive(Subcommand)]
enum ReviewCmd {
    /// Accept a proposed claim.
    Accept { id: String },
    /// Reject a proposed claim; the reason is kept forever.
    Reject {
        id: String,
        #[arg(long)]
        reason: String,
    },
}

#[derive(Subcommand)]
enum DaemonCmd {
    /// Print the token to paste into the browser extension.
    Token,
}

#[derive(Subcommand)]
enum NodeCmd {
    /// Pair this device with a relay using its one-time bootstrap code.
    Login {
        /// Public relay URL, for example https://ctx.example.com.
        #[arg(long)]
        relay: String,
        /// One-time code printed by `ctx relay init`.
        #[arg(long)]
        code: String,
    },
    /// Keep this local context store available through its paired relay.
    Start {
        /// Replace the saved relay URL before connecting.
        #[arg(long)]
        relay: Option<String>,
        /// Change the local context branch served to remote MCP clients.
        #[arg(long)]
        branch: Option<String>,
    },
    /// Show the paired relay and this device's public id.
    Status,
}

#[derive(Subcommand)]
enum RelayCmd {
    /// Create a relay data directory and print its one-time setup secrets.
    Init {
        /// Directory that holds the relay's small identity/routing database.
        #[arg(long, default_value = "./recuros-relay")]
        data: PathBuf,
    },
    /// Serve the relay. Put TLS/reverse proxying in front before public use.
    Serve {
        #[arg(long, default_value = "./recuros-relay")]
        data: PathBuf,
        #[arg(long, default_value = ctx_relay::DEFAULT_RELAY_ADDR)]
        addr: String,
    },
    /// Create an additional revocable MCP connector secret.
    Token {
        #[arg(long, default_value = "./recuros-relay")]
        data: PathBuf,
        #[arg(long, default_value = "connector")]
        label: String,
    },
    /// List or revoke devices enrolled with this relay.
    #[command(subcommand)]
    Device(RelayDeviceCmd),
}

#[derive(Subcommand)]
enum RelayDeviceCmd {
    /// List device ids and their active/revoked state.
    Ls {
        #[arg(long, default_value = "./recuros-relay")]
        data: PathBuf,
    },
    /// Immediately deny future relayed requests to this device.
    Revoke {
        id: String,
        #[arg(long, default_value = "./recuros-relay")]
        data: PathBuf,
    },
}

#[derive(Subcommand)]
enum HookCmd {
    /// Pull, refresh AGENTS.md, and print context that changed since it was loaded.
    SessionStart,
}

#[derive(Clone, Copy, ValueEnum)]
enum Verdict {
    Up,
    Down,
}

#[derive(Clone, Copy, ValueEnum)]
enum KindArg {
    Fact,
    Decision,
    Rejected,
    Constraint,
    Question,
    Claim,
}

impl From<KindArg> for Kind {
    fn from(k: KindArg) -> Kind {
        match k {
            KindArg::Fact => Kind::Fact,
            KindArg::Decision => Kind::Decision,
            KindArg::Rejected => Kind::Rejected,
            KindArg::Constraint => Kind::Constraint,
            KindArg::Question => Kind::Question,
            KindArg::Claim => Kind::Claim,
        }
    }
}

#[derive(Clone, Copy, ValueEnum)]
enum ConfidenceArg {
    High,
    Medium,
    Low,
}

impl From<ConfidenceArg> for Confidence {
    fn from(c: ConfidenceArg) -> Confidence {
        match c {
            ConfidenceArg::High => Confidence::High,
            ConfidenceArg::Medium => Confidence::Medium,
            ConfidenceArg::Low => Confidence::Low,
        }
    }
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    // Hooks must never break an agent session.
    let is_hook = matches!(cli.command, Command::Hook(_));
    match run(cli) {
        Ok(code) => code,
        Err(e) if is_hook => {
            eprintln!("ctx hook: {e:#}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("error: {e:#}");
            ExitCode::FAILURE
        }
    }
}

fn cwd() -> PathBuf {
    std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."))
}

fn open(home: &CtxHome) -> Result<App> {
    App::open(home.clone(), Some(&cwd()))
}

fn run(cli: Cli) -> Result<ExitCode> {
    let home = CtxHome::locate()?;
    match cli.command {
        Command::Init {
            project,
            branch,
            agents,
            no_agents,
        } => init(&home, project, branch, &agents, no_agents, &cwd())?,
        Command::New { name } => flow::new(&home, &name)?,
        Command::Spec(cmd) => {
            let mut app = open(&home)?;
            match cmd {
                SpecCmd::Save {
                    file,
                    paste,
                    name,
                    title,
                    to,
                } => {
                    // A file is the spec: take it literally. Pasted or piped
                    // text comes from a chat, so a ````ctx-spec fence around it
                    // is unwrapped. Unwrapping a file would be wrong for any
                    // document that merely *contains* such a fence, like our own
                    // docs/protocol.md.
                    let (text, unwrap) = match (paste, file.as_deref()) {
                        (true, _) => (clipboard::paste()?, true),
                        (false, Some(p)) if p == Path::new("-") => (read_stdin()?, true),
                        (false, Some(p)) => (
                            std::fs::read_to_string(p)
                                .with_context(|| format!("reading {}", p.display()))?,
                            false,
                        ),
                        (false, None) if !io::stdin().is_terminal() => (read_stdin()?, true),
                        (false, None) => bail!("give a file, `-` for stdin, or --paste"),
                    };
                    let branch = app.resolve_branch(to.as_deref())?;
                    flow::save(&mut app, &branch, &text, &name, title.as_deref(), unwrap)?;
                }
                SpecCmd::Show { name, branch } => {
                    let b = app.resolve_branch(branch.as_deref())?;
                    flow::show(&app, &b, &name)?;
                }
                SpecCmd::Ls { branch } => {
                    let b = app.resolve_branch(branch.as_deref())?;
                    flow::list(&app, &b)?;
                }
            }
        }
        Command::Build {
            name,
            dir,
            agents,
            no_agents,
        } => flow::build(&home, &name, dir.as_deref(), &agents, no_agents)?,
        Command::Map {
            project,
            per_branch,
            out,
        } => {
            let app = open(&home)?;
            let map = flow::map(&app, project.as_deref(), per_branch)?;
            match out {
                Some(p) => {
                    std::fs::write(&p, &map).with_context(|| format!("writing {}", p.display()))?;
                    eprintln!(
                        "wrote {} (renders on GitHub and in most markdown viewers)",
                        p.display()
                    );
                }
                None => print!("{map}"),
            }
        }
        Command::Save {
            text,
            kind,
            why,
            refs,
            tags,
            confidence,
            to,
            supersedes,
            paste,
            src,
        } => {
            let mut app = open(&home)?;
            let branch = app.resolve_branch(to.as_deref())?;
            if paste {
                let block = clipboard::paste()?;
                let inputs = ctx_app::claims_block::parse(&block)?;
                let report = app.save_inputs(&inputs, &branch, "clipboard");
                for s in &report.saved {
                    println!("saved {} -> {branch}", tag(&s.cid));
                }
                for d in &report.duplicates {
                    println!("already recorded {}", tag(&d.cid));
                }
                for e in &report.errors {
                    eprintln!("skipped: {e}");
                }
            } else {
                let text = match text.as_deref() {
                    Some("-") => read_stdin()?,
                    None if !io::stdin().is_terminal() => read_stdin()?,
                    Some(t) => t.to_owned(),
                    None => {
                        bail!("nothing to save: `ctx save \"your claim\"`, or `ctx save --paste`")
                    }
                };
                let mut draft = ClaimDraft::new(kind.into(), text);
                draft.branch = branch;
                draft.why = why;
                draft.refs = refs;
                draft.entities = tags;
                draft.confidence = confidence.into();
                draft.src = src;
                if let Some(s) = supersedes {
                    draft.supersedes = Some(app.find_one(&s)?.id);
                }
                match app.save(draft)? {
                    SaveOutcome::Saved(c) => {
                        println!("saved {} {} -> {}", tag(&c.cid), c.kind, c.branch)
                    }
                    SaveOutcome::Duplicate(c) => {
                        println!("already recorded as {} ({})", tag(&c.cid), c.id)
                    }
                }
            }
            // Keep this repo's AGENTS.md current; never fail a save over it.
            if let Err(e) = app.refresh_agents_md() {
                eprintln!("warning: could not refresh AGENTS.md: {e:#}");
            }
        }
        Command::Search {
            query,
            branch,
            kind,
            limit,
            verbose,
            all,
        } => {
            let app = open(&home)?;
            let filter = Filter {
                branch: branch
                    .as_deref()
                    .map(|b| app.resolve_branch(Some(b)))
                    .transpose()?,
                kinds: kind.into_iter().map(Kind::from).collect(),
                // Deleted claims stay in the log, and after a rename the log
                // holds a retired copy of everything. Matching them by
                // default would bury every hit under its own history.
                statuses: if all {
                    Vec::new()
                } else {
                    vec![
                        Status::Active,
                        Status::Proposed,
                        Status::Superseded,
                        Status::Rejected,
                    ]
                },
                limit: Some(limit),
                ..Default::default()
            };
            let hits = app.store.search(&query.join(" "), &filter)?;
            if hits.is_empty() {
                eprintln!("no matches");
            }
            for c in &hits {
                print_claim(c, verbose, true);
            }
        }
        Command::Pack {
            branch,
            task,
            budget,
            projection,
            clip,
            out,
        } => {
            let app = open(&home)?;
            let projection = projection
                .as_deref()
                .map(|p| p.parse::<Projection>().map_err(anyhow::Error::msg))
                .transpose()?;
            let budget = match (budget, projection) {
                (Some(b), _) => Some(b),
                (None, Some(Projection::Prose)) => Some(UNLIMITED),
                _ => None,
            };
            let pack = app.pack(&PackOpts {
                branch,
                task,
                budget,
                projection,
            })?;
            if let Some(path) = out {
                let is_agents = path
                    .file_name()
                    .is_some_and(|n| n.eq_ignore_ascii_case("AGENTS.md"));
                let content = if is_agents {
                    ctx_app::agents_md::splice(
                        &std::fs::read_to_string(&path).unwrap_or_default(),
                        &pack.markdown,
                    )
                } else {
                    pack.markdown.clone()
                };
                std::fs::write(&path, content)
                    .with_context(|| format!("writing {}", path.display()))?;
                eprintln!(
                    "wrote {} ({} claims, ~{} tokens)",
                    path.display(),
                    pack.claims.len(),
                    pack.tokens
                );
            } else if clip {
                clipboard::copy(&pack.markdown)?;
                eprintln!(
                    "copied {} claims (~{} tokens) to the clipboard: paste it into any chat",
                    pack.claims.len(),
                    pack.tokens
                );
            } else {
                print!("{}", pack.markdown);
            }
        }
        Command::Use { branch } => {
            let mut app = open(&home)?;
            // Outside a repo, a bare name is an idea: `ctx use habit` means
            // habit/research, where chat research lives.
            let name = branch.trim();
            let b = if app.binding.is_none() && !name.contains('/') && name != BranchRef::DEFAULT {
                BranchRef::new(&format!("{}/research", slug(name)))?
            } else {
                app.resolve_branch(Some(name))?
            };
            let known = app.branches.contains(&b)
                || app.store.branches()?.iter().any(|s| s.branch == b.as_str());
            app.set_active(&b)?;
            println!("active branch: {b}");
            if !known {
                eprintln!("note: `{b}` has nothing saved yet; chats will start saving there");
            }
            sync_now(&mut app, false)?;
        }
        Command::Status => status(&home)?,
        Command::Commands => print!("{}", ctx_app::COMMANDS_MD),
        Command::Branch(cmd) => branch_cmd(&home, cmd)?,
        Command::Review { action } => review(&home, action)?,
        Command::Show { id } => {
            let app = open(&home)?;
            let c = app.find_one(&id)?;
            print_claim(&c, true, true);
            for h in app.store.status_history(c.id)? {
                println!(
                    "{}{}  → {}{}",
                    " ".repeat(20),
                    h.t_tx.format("%Y-%m-%d"),
                    h.to,
                    h.reason.map(|r| format!(": {r}")).unwrap_or_default()
                );
            }
        }
        Command::Onboard { clip } => onboard(&home, clip)?,
        Command::List { all } => list(&home, all)?,
        Command::Rename { from, to, yes } => rename(&home, &from, &to, yes)?,
        Command::Remove { target, yes, cloud } => remove(&home, &target, yes, cloud)?,
        Command::Archive { id, reason } => {
            let mut app = open(&home)?;
            let c = app.find_one(&id)?;
            app.transition(&c, Status::Archived, reason, "cli")?;
            println!(
                "archived {} (still in the log; hidden from packs)",
                tag(&c.cid)
            );
            let _ = app.refresh_agents_md();
        }
        Command::Rate { id, verdict } => {
            let mut app = open(&home)?;
            let c = app.find_one(&id)?;
            app.rate(&c, matches!(verdict, Verdict::Up))?;
            println!("noted {}", tag(&c.cid));
        }
        Command::Refine { branch, threshold } => refine(&home, branch, threshold)?,
        Command::Sync => {
            // (SPEC.md is refreshed below, after pulling new research.)
            let mut app = open(&home)?;
            let r = app.sync()?;
            match &r.remote {
                None => println!(
                    "committed locally{} (no remote yet; to sync: git -C \"{}\" remote add origin <private repo url>)",
                    if r.committed { "" } else { ": nothing new" },
                    home.root().display()
                ),
                Some(url) => println!("synced with {url}"),
            }
            let _ = app.refresh_agents_md();
            let _ = app.refresh_spec_file();
        }
        Command::Verify { root, branch } => {
            let app = open(&home)?;
            let b = app.resolve_branch(branch.as_deref())?;
            match root {
                None => println!("{}  {b}", app.state_root(&b)?),
                Some(r) => match app.verify(&b, &r)? {
                    VerifyResult::InSync => println!("in-sync"),
                    VerifyResult::Behind(n) => {
                        println!("behind: that side is missing {n} claim(s) this machine has")
                    }
                    VerifyResult::AheadOrDiverged => {
                        println!(
                            "ahead-or-diverged: that side has claims this machine hasn't seen; run `ctx sync`"
                        )
                    }
                },
            }
        }
        Command::Reindex => {
            if !home.exists() {
                bail!("no RecurOS store at {}", home.root().display());
            }
            let start = Instant::now();
            let mut app = App::open(home.clone(), None)?;
            app.reindex()?;
            let stats = app.store.stats()?;
            println!(
                "reindexed {} claims from {} shards in {:.0?}",
                stats.claims,
                stats.shards,
                start.elapsed()
            );
        }
        Command::Log {
            branch,
            kind,
            since,
            limit,
            verbose,
            all,
        } => {
            let app = open(&home)?;
            let filter = Filter {
                branch: branch
                    .as_deref()
                    .map(|b| app.resolve_branch(Some(b)))
                    .transpose()?,
                kinds: kind.into_iter().map(Kind::from).collect(),
                since: since.map(|d| {
                    DateTime::<Utc>::from_naive_utc_and_offset(d.and_time(Default::default()), Utc)
                }),
                limit: (limit > 0).then_some(limit),
                statuses: if all {
                    Vec::new()
                } else {
                    vec![
                        Status::Active,
                        Status::Proposed,
                        Status::Superseded,
                        Status::Rejected,
                    ]
                },
            };
            let claims = app.store.scan(&filter)?;
            if claims.is_empty() {
                eprintln!("no claims yet: `ctx save \"...\"` records one");
            }
            for c in &claims {
                print_claim(c, verbose, branch.is_none());
            }
        }
        Command::Eval {
            fixtures,
            model,
            budget,
            ollama,
            example,
        } => {
            let path = fixtures.unwrap_or_else(|| home.refs_dir().join("eval.yaml"));
            if example {
                if path.exists() {
                    bail!("{} already exists", path.display());
                }
                if let Some(dir) = path.parent() {
                    std::fs::create_dir_all(dir)?;
                }
                std::fs::write(&path, eval::EXAMPLE)?;
                println!(
                    "wrote {}: edit it with real questions, then run `ctx eval`",
                    path.display()
                );
                return Ok(ExitCode::SUCCESS);
            }
            let app = open(&home)?;
            eval::run(
                &app,
                &path,
                &eval::Options {
                    budgets: &budget,
                    model: model.as_deref(),
                    ollama: &ollama,
                    projection: Projection::Markdown,
                },
            )?;
        }
        Command::Doctor => {
            if !doctor::run(home, &cwd()) {
                return Ok(ExitCode::FAILURE);
            }
        }
        Command::Mcp => ctx_mcp::serve_stdio(open(&home)?)?,
        Command::Daemon { action, addr } => match action {
            Some(DaemonCmd::Token) => println!("{}", ctx_daemon::ensure_token()?),
            None => {
                let app = App::open(home.clone(), None)?;
                let h = home.clone();
                ctx_daemon::spawn_idle_sync(move || App::open(h.clone(), None));
                ctx_daemon::ensure_token()?;
                println!("browser extension token: run `ctx daemon token` to print it");
                ctx_daemon::serve(app, &addr)?;
            }
        },
        Command::Node(NodeCmd::Login { relay, code }) => {
            let branch = open(&home)?.default_branch()?;
            let device = ctx_relay::node_login(&home, &relay, &code, &branch)?;
            println!("paired this device as {device}");
            println!("it will serve branch {branch}");
            println!("start it when remote MCP access is wanted: ctx node start");
        }
        Command::Node(NodeCmd::Start { relay, branch }) => {
            ctx_relay::node_start(home, relay.as_deref(), branch.as_deref())?;
        }
        Command::Node(NodeCmd::Status) => match ctx_relay::node_status(&home)? {
            Some((relay, device, branch)) => {
                println!("paired relay: {relay}");
                println!("device: {device}");
                println!("serving branch: {branch}");
                println!("start remote access: ctx node start");
            }
            None => println!(
                "node is not paired; run `ctx node login --relay URL --code BOOTSTRAP_CODE`"
            ),
        },
        Command::Relay(RelayCmd::Init { data }) => {
            let init = ctx_relay::init(&data)?;
            println!("relay initialised in {}", data.display());
            println!(
                "\nBootstrap code (shown only once; pair the first node with it):\n{}",
                init.bootstrap_code
            );
            println!(
                "\nConnector secret (shown only once; send it as Authorization: Bearer to /mcp):\n{}",
                init.connector_secret
            );
            println!("\nRun: ctx relay serve --data {}", data.display());
        }
        Command::Relay(RelayCmd::Serve { data, addr }) => ctx_relay::serve(data, &addr)?,
        Command::Relay(RelayCmd::Token { data, label }) => {
            let secret = ctx_relay::create_connector_secret(&data, &label)?;
            println!("connector secret (shown only once): {secret}");
        }
        Command::Relay(RelayCmd::Device(RelayDeviceCmd::Ls { data })) => {
            let devices = ctx_relay::devices(&data)?;
            if devices.is_empty() {
                println!("no devices enrolled");
            }
            for device in devices {
                let active = if device.active { "active" } else { "inactive" };
                let revoked = if device.revoked { ", revoked" } else { "" };
                println!("{}  {active}{revoked}", device.id);
            }
        }
        Command::Relay(RelayCmd::Device(RelayDeviceCmd::Revoke { id, data })) => {
            ctx_relay::revoke_device(&data, &id)?;
            println!("revoked device {id}");
        }
        Command::Hook(HookCmd::SessionStart) => session_start(&home)?,
    }
    Ok(ExitCode::SUCCESS)
}

/// Ask before hiding more than one claim. Non-interactive callers must
/// pass --yes, so a script can never delete an idea by accident.
fn confirm(question: &str, yes: bool) -> Result<bool> {
    if yes {
        return Ok(true);
    }
    if !io::stdin().is_terminal() {
        bail!("{question} Re-run with --yes to confirm.");
    }
    eprint!("{question} [y/N] ");
    let mut answer = String::new();
    io::stdin().read_line(&mut answer)?;
    Ok(matches!(answer.trim(), "y" | "Y" | "yes"))
}

fn plural(n: usize, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// Push a change that affects what chats see (delete, use, new) to the
/// cloud right away. Saving stays offline; these don't, because a stale
/// cloud makes ChatGPT and claude.ai write into the wrong idea. With
/// `strict`, failing to reach the cloud is an error instead of a warning.
pub(crate) fn sync_now(app: &mut App, strict: bool) -> Result<()> {
    match app.sync() {
        Ok(r) if r.remote.is_some() => {
            println!("synced: ChatGPT and claude.ai see this now");
            Ok(())
        }
        Ok(_) if strict => {
            bail!("done on this machine, but there's no git remote, so the cloud can't see it")
        }
        Ok(_) => Ok(()), // local-only store: nothing to reach
        Err(e) if strict => Err(e.context(
            "done on this machine, but couldn't reach the cloud; run `ctx sync` when online",
        )),
        Err(e) => {
            eprintln!(
                "warning: couldn't sync ({e:#}); chats will see this after your next `ctx sync`"
            );
            Ok(())
        }
    }
}

/// `ctx onboard`: the prompt that fills this repo's context from this repo.
fn onboard(home: &CtxHome, clip: bool) -> Result<()> {
    let app = open(home)?;
    let Some(binding) = &app.binding else {
        bail!("this directory isn't wired yet. Run `ctx init` first");
    };
    let prompt = flow::onboard_prompt(&binding.root, &binding.branch_ref()?);
    if clip {
        clipboard::copy(&prompt)?;
        println!("copied. Paste it into Claude Code, Cursor, Codex or Gemini CLI.");
    } else {
        print!("{prompt}");
    }
    Ok(())
}

/// `ctx list`: the ideas you still have, and what each one would cost to
/// delete. Deleted ideas are names only, and only with `-a`: their branch
/// breakdown is all zeroes and it buries the ideas you actually work on.
fn list(home: &CtxHome, all: bool) -> Result<()> {
    let app = open(home)?;
    let active = app.active_ref();
    let bound = app.binding.as_ref().map(|b| b.project.clone());
    let mut names: BTreeSet<String> = app.branches.all().into_iter().map(String::from).collect();
    for c in app.store.branches()? {
        names.insert(c.branch);
    }
    // Branch summaries come from claims, so a branch holding only documents
    // is invisible without this: exactly how a mis-saved spec sat in the
    // store where nobody could see it to delete it.
    for d in app.store.docs(None)? {
        names.insert(d.branch.to_string());
    }

    let mut projects: BTreeMap<String, Vec<(BranchRef, usize, usize, bool)>> = BTreeMap::new();
    for b in names
        .iter()
        .map(|n| BranchRef::new(n))
        .collect::<Result<Vec<_>, _>>()?
    {
        let Some((project, _)) = b.as_str().split_once('/') else {
            continue;
        };
        let (claims, docs) = app.removal_counts(&b)?;
        let archived = app.branches.get(&b).is_some_and(|d| d.archived);
        projects
            .entry(project.to_owned())
            .or_default()
            .push((b, claims, docs, archived));
    }

    // An idea is gone when every branch of it is archived and nothing of it
    // is still visible.
    let (gone, live): (Vec<_>, Vec<_>) = projects.into_iter().partition(|(_, branches)| {
        branches
            .iter()
            .all(|(_, claims, docs, archived)| *archived && claims + docs == 0)
    });

    if live.is_empty() && (gone.is_empty() || !all) {
        println!("no ideas yet. Start one with: ctx new \"<your idea>\"");
        return Ok(());
    }

    let width = live
        .iter()
        .flat_map(|(_, bs)| bs)
        .map(|(b, ..)| b.as_str().len())
        .max()
        .unwrap_or(24)
        .clamp(24, 44);
    for (project, branches) in &live {
        let here = (bound.as_deref() == Some(project.as_str())).then_some("  (this repo)");
        let (claims, docs): (usize, usize) = branches
            .iter()
            .fold((0, 0), |(c, d), (_, bc, bd, _)| (c + bc, d + bd));
        println!("{project}{}", here.unwrap_or(""));
        for (b, claims, docs, archived) in branches {
            let mark = if active.as_ref() == Some(b) { "*" } else { " " };
            let mut note = String::new();
            if *docs > 0 {
                note.push_str(&format!(", {}", plural(*docs, "document", "documents")));
            }
            if *archived {
                note.push_str("  [deleted]");
            }
            println!(
                "  {mark} {:<width$} {}{note}",
                b.as_str(),
                plural(*claims, "claim", "claims")
            );
        }
        // The whole-idea command, spelled out per idea, because the thing
        // people want to delete is usually the idea and not one branch of it.
        if claims + docs > 0 {
            println!(
                "    delete the whole idea:  ctx delete {project} --cloud   \
({}, {})",
                plural(claims, "claim", "claims"),
                plural(docs, "document version", "document versions")
            );
        } else {
            println!("    empty:  ctx delete {project} --cloud");
        }
        println!();
    }

    if !live.is_empty() {
        println!("Deleting hides it here, on GitHub, and in ChatGPT and claude.ai.");
        println!("Nothing leaves the log: `ctx log --all` still shows it.");
    }
    if !gone.is_empty() {
        if all {
            let names: Vec<&str> = gone.iter().map(|(p, _)| p.as_str()).collect();
            println!(
                "\n{}: {}",
                plural(names.len(), "deleted idea", "deleted ideas"),
                names.join(", ")
            );
            println!("Still in the log: `ctx log --all --branch <idea>/<lane>`.");
        } else {
            println!(
                "{} hidden; `ctx list -a` names them.",
                plural(gone.len(), "deleted idea", "deleted ideas")
            );
        }
    }
    Ok(())
}

/// `ctx rename`: the same context under a new name.
fn rename(home: &CtxHome, from: &str, to: &str, yes: bool) -> Result<()> {
    let mut app = open(home)?;
    let (from, to) = (slug(from), slug(to));
    let branches = app.project_branches(&from)?;
    if branches.is_empty() {
        bail!("no idea named `{from}`");
    }
    let (mut claims, mut docs) = (0, 0);
    for b in &branches {
        let (c, d) = app.removal_counts(b)?;
        claims += c;
        docs += d;
    }
    let question = format!(
        "Rename {from} to {to} ({}, {})? Nothing is lost; the old name is kept as history.",
        plural(claims, "claim", "claims"),
        plural(docs, "document version", "document versions")
    );
    if !confirm(&question, yes)? {
        println!("nothing renamed");
        return Ok(());
    }
    let moved = app.rename_project(&from, &to)?;
    println!(
        "renamed {from} to {to}: {}, {} across {}",
        plural(moved.claims, "claim", "claims"),
        plural(moved.docs, "document", "documents"),
        plural(moved.branches, "branch", "branches")
    );
    // Any repo bound to the old name should follow it.
    if let Some(binding) = app.binding.clone()
        && binding.project == from
    {
        let moved = Binding {
            project: to.clone(),
            ..binding
        };
        moved.write(&moved.root)?;
        app.binding = Some(moved.clone());
        println!("  this repo now uses {to}/{}", moved.branch);
    }
    if let Some(block) = app.refresh_agents_md()? {
        let _ = block;
        println!("  AGENTS.md updated");
    }
    sync_now(&mut app, false)?;
    println!("  synced: ChatGPT and claude.ai see the new name");
    Ok(())
}

fn remove(home: &CtxHome, target: &str, yes: bool, cloud: bool) -> Result<()> {
    let mut app = open(home)?;
    let t = target.trim();
    let is_claim = t.starts_with("c:")
        || t.starts_with("b3:")
        || (t.len() == 26 && ulid::Ulid::from_string(t).is_ok());
    if is_claim {
        let c = app.find_one(t)?;
        if c.status == Status::Archived {
            println!("{} is already deleted", tag(&c.cid));
            return Ok(());
        }
        app.transition(&c, Status::Archived, Some("removed".into()), "cli")?;
        println!("deleted {} \"{}\"", tag(&c.cid), first_line(&c.text));
    } else if t.contains('/') {
        let b = BranchRef::new(t)?;
        let (claims, docs) = app.removal_counts(&b)?;
        if claims + docs == 0 && !app.branches.contains(&b) {
            bail!("no branch named `{t}`");
        }
        let what = format!(
            "{}, {}",
            plural(claims, "claim", "claims"),
            plural(docs, "document version", "document versions")
        );
        if !confirm(&format!("Delete branch {b} ({what})?"), yes)? {
            println!("nothing deleted");
            return Ok(());
        }
        let (claims, docs) = app.remove_branch(&b)?;
        println!(
            "deleted {b}: {}, {}",
            plural(claims, "claim", "claims"),
            plural(docs, "document version", "document versions")
        );
    } else {
        let project = slug(t);
        let branches = app.project_branches(&project)?;
        if branches.is_empty() {
            bail!("no idea named `{project}`. To delete one claim, use its tag: ctx delete c:xxxx");
        }
        let (mut claims, mut docs) = (0, 0);
        for b in &branches {
            let (c, d) = app.removal_counts(b)?;
            claims += c;
            docs += d;
        }
        let names: Vec<String> = branches.iter().map(|b| b.to_string()).collect();
        if !confirm(
            &format!(
                "Delete the idea `{project}` ({}: {}, {})?",
                names.join(", "),
                plural(claims, "claim", "claims"),
                plural(docs, "document version", "document versions")
            ),
            yes,
        )? {
            println!("nothing deleted");
            return Ok(());
        }
        let (n, claims, docs) = app.remove_project(&project)?;
        println!(
            "deleted {project}: {}, {}, {}",
            plural(n, "branch", "branches"),
            plural(claims, "claim", "claims"),
            plural(docs, "document version", "document versions")
        );
    }
    println!(
        "gone from packs, search and AGENTS.md; still in the history (the log is never erased)"
    );
    let _ = app.refresh_agents_md();
    sync_now(&mut app, cloud)?;
    Ok(())
}

fn read_stdin() -> Result<String> {
    let mut buf = String::new();
    io::stdin()
        .read_to_string(&mut buf)
        .context("reading claim from stdin")?;
    Ok(buf)
}

/// `[c:7f2a]` from a full cid.
fn tag(cid: &str) -> String {
    let hex = cid.strip_prefix("b3:").unwrap_or(cid);
    format!("[c:{}]", &hex[..hex.len().min(4)])
}

fn print_claim(c: &Claim, verbose: bool, show_branch: bool) {
    let more = if c.text.contains('\n') { " …" } else { "" };
    let status = if c.status == Status::Active {
        String::new()
    } else {
        format!(" ({})", c.status)
    };
    let branch = if show_branch && c.branch.as_str() != BranchRef::DEFAULT {
        format!("  · {}", c.branch)
    } else {
        String::new()
    };
    println!(
        "{} {}  {:<10}  {}{more}{status}{branch}",
        c.t_tx.format("%Y-%m-%d"),
        tag(&c.cid),
        c.kind,
        first_line(&c.text),
    );
    if verbose {
        let pad = " ".repeat(20);
        for line in c.text.lines().skip(1) {
            println!("{pad}{line}");
        }
        if let Some(why) = &c.why {
            println!("{pad}why:  {}", why.replace('\n', " "));
        }
        if !c.refs.is_empty() {
            println!("{pad}refs: {}", c.refs.join(", "));
        }
        if !c.entities.is_empty() {
            println!("{pad}tags: {}", c.entities.join(", "));
        }
        println!(
            "{pad}{} · {} · {} · {} · from {}",
            c.id, c.branch, c.status, c.confidence, c.src
        );
    }
}

fn repo_root(start: &Path) -> PathBuf {
    start
        .ancestors()
        .find(|d| d.join(".git").exists())
        .unwrap_or(start)
        .to_owned()
}

fn slug(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() || c == '_' {
            out.push(c);
        } else if !out.ends_with('-') && !out.is_empty() {
            out.push('-');
        }
    }
    let out = out.trim_matches('-').to_owned();
    if out.is_empty() {
        "project".into()
    } else {
        out
    }
}

fn init(
    home: &CtxHome,
    project: Option<String>,
    branch: Option<String>,
    agents: &[String],
    no_agents: bool,
    dir: &Path,
) -> Result<()> {
    let root = repo_root(dir);
    let existing = Binding::discover(&root)?.filter(|b| b.root == root);
    let project = slug(
        &project
            .or_else(|| existing.as_ref().map(|b| b.project.clone()))
            .unwrap_or_else(|| {
                root.file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default()
            }),
    );
    let branch = slug(
        &branch
            .or_else(|| existing.as_ref().map(|b| b.branch.clone()))
            .unwrap_or_else(|| "code".into()),
    );
    let created_store = !home.exists();

    let binding = Binding {
        project: project.clone(),
        branch: branch.clone(),
        root: root.clone(),
        legacy: false,
    };
    if existing.as_ref().map(|b| (&b.project, &b.branch)) != Some((&project, &branch)) {
        binding.write(&root)?;
    }
    let mut app = App::open(home.clone(), Some(&root))?;
    app.ensure_project(&project, &branch)?;
    let _ = ctx_daemon::ensure_token();
    app.refresh_agents_md()?;
    let spec = app.refresh_spec_file()?;

    println!("RecurOS: {project}/{branch}");
    if created_store {
        println!("  created store at {}", home.root().display());
    }
    println!(
        "  ✓ .ctx/       config.yaml binds this repo to {project}/{branch}; commands.md for agents (commit it)"
    );
    println!("  ✓ AGENTS.md   RecurOS section added; your own content is kept (commit it)");
    match spec {
        ctx_app::SpecFile::Written | ctx_app::SpecFile::Unchanged => {
            println!("  ✓ .ctx/SPEC.md  the spec from {project}'s research")
        }
        ctx_app::SpecFile::LeftAlone => {
            println!("  ! .ctx/SPEC.md  was edited by hand; left as is")
        }
        ctx_app::SpecFile::NoSpec => {}
    }

    if !no_agents {
        let user_home = std::env::home_dir().unwrap_or_default();
        let chosen: Vec<Agent> = if agents.is_empty() {
            Agent::ALL
                .into_iter()
                .filter(|a| a.detect(&user_home))
                .collect()
        } else {
            agents
                .iter()
                .map(|a| {
                    Agent::parse(a).with_context(|| {
                        format!("unknown agent `{a}` (claude-code, codex, cursor, gemini)")
                    })
                })
                .collect::<Result<_>>()?
        };
        let (command, on_path) = ctx_wire::ctx_command();
        if chosen.is_empty() {
            println!("  · no agents detected; AGENTS.md alone works with most of them");
        }
        for agent in chosen {
            println!("  {agent}:");
            for step in ctx_wire::wire(agent, &root, &user_home, &command) {
                println!("    {}", step.to_string().replace('\n', "\n    "));
            }
        }
        if !on_path {
            println!(
                "\n  note: `ctx` isn't on your PATH, so configs point at {command}\n        \
                 put ctx on PATH (see README) and re-run `ctx init` before committing them"
            );
        }
    }

    println!("\nNext:");
    // An existing repository already holds most of its own context. Point at
    // the command that gets it out, rather than at an empty store.
    if BranchRef::new(&format!("{project}/{branch}"))
        .is_ok_and(|b| app.pool(&b).is_ok_and(|p| p.is_empty()))
    {
        println!("  ctx onboard       # a prompt that fills this from your own code");
    }
    println!("  ctx save \"Use SQLite, not Postgres\" -k decision -w \"no server to run\"");
    println!("  ctx pack          # see exactly what your agents will see");
    if ctx_git::sync::remote_url(home).is_none() {
        println!(
            "  sync across machines: create a PRIVATE git repo, then\n    \
             git -C \"{}\" remote add origin <url> && ctx sync",
            home.root().display()
        );
    }
    Ok(())
}

fn status(home: &CtxHome) -> Result<()> {
    let app = open(home)?;
    let stats = app.store.stats()?;
    println!("store    {}", home.root().display());
    println!(
        "remote   {}",
        ctx_git::sync::remote_url(home).unwrap_or_else(|| "none (local only)".into())
    );
    match &app.binding {
        Some(b) => println!("repo     {} → {}", b.root.display(), b.branch_ref()?),
        None => println!("repo     not bound (run `ctx init` in a project)"),
    }
    println!("branch   {}", app.default_branch()?);
    match ctx_relay::node_status(home)? {
        Some((relay, device, branch)) => println!("node     {device} via {relay} ({branch})"),
        None => {
            println!("node     not paired (optional: `ctx node login --relay URL --code CODE`)")
        }
    }
    println!("claims   {} total", stats.claims);
    for (k, n) in &stats.by_kind {
        println!("         {n:>5} {k}");
    }
    let pending = app.proposals(None)?.len();
    if pending > 0 {
        println!("review   {pending} pending proposal(s): `ctx review`");
    }
    Ok(())
}

fn review(home: &CtxHome, action: Option<ReviewCmd>) -> Result<()> {
    let mut app = open(home)?;
    match action {
        None => {
            let pending = app.proposals(None)?;
            if pending.is_empty() {
                println!("no pending proposals");
                return Ok(());
            }
            for c in &pending {
                println!(
                    "{}  {} → {}  (from {})",
                    tag(&c.cid),
                    c.kind,
                    c.branch,
                    c.src
                );
                println!("    {}", c.text.replace('\n', "\n    "));
                if let Some(w) = &c.why {
                    println!("    why: {w}");
                }
            }
            println!(
                "\naccept: ctx review accept c:XXXX    reject: ctx review reject c:XXXX --reason \"...\""
            );
        }
        Some(ReviewCmd::Accept { id }) => {
            let c = app.find_one(&id)?;
            if c.status != Status::Proposed {
                bail!("{} is {}, not proposed", tag(&c.cid), c.status);
            }
            app.transition(&c, Status::Active, None, "review")?;
            println!("accepted {} into {}", tag(&c.cid), c.branch);
        }
        Some(ReviewCmd::Reject { id, reason }) => {
            let c = app.find_one(&id)?;
            if c.status != Status::Proposed {
                bail!("{} is {}, not proposed", tag(&c.cid), c.status);
            }
            app.transition(&c, Status::Rejected, Some(reason), "review")?;
            println!("rejected {} (the reason is kept)", tag(&c.cid));
        }
    }
    Ok(())
}

fn branch_cmd(home: &CtxHome, cmd: BranchCmd) -> Result<()> {
    let mut app = open(home)?;
    match cmd {
        BranchCmd::New {
            name,
            parent,
            template,
            inherits,
        } => {
            let b = app.resolve_branch(Some(&name))?;
            if !b.as_str().contains('/') {
                bail!(
                    "branches live in a project: use `project/{name}`, or run inside a bound repo"
                );
            }
            let inherits = inherits
                .iter()
                .map(|s| ctx_app::parse_inherits(s))
                .collect::<Result<Vec<_>>>()?;
            app.branch_new(&b, parent.as_deref(), template.as_deref(), &inherits)?;
            println!("created {b}");
        }
        BranchCmd::Ls => {
            let counts = app.store.branches()?;
            let active = app.default_branch()?;
            let mut names: Vec<String> = app.branches.all().into_iter().map(String::from).collect();
            for c in &counts {
                if !names.contains(&c.branch) {
                    names.push(c.branch.clone());
                }
            }
            names.sort();
            if names.is_empty() {
                println!("no branches yet: `ctx init` in a project creates research and code");
            }
            for n in names {
                let r = BranchRef::new(&n)?;
                let def = app.branches.get(&r);
                let c = counts.iter().find(|c| c.branch == n);
                let marker = if r == active { "*" } else { " " };
                let parent = def
                    .and_then(|d| d.parent.as_deref())
                    .map(|p| format!("  (inherits from {p})"))
                    .unwrap_or_default();
                let archived = if def.is_some_and(|d| d.archived) {
                    "  [archived]"
                } else {
                    ""
                };
                let last = c
                    .and_then(|c| c.last_tx)
                    .map(|t| t.format("%Y-%m-%d").to_string())
                    .unwrap_or_else(|| "-".into());
                println!(
                    "{marker} {n:<28} {:>5} claims {:>3} proposed   last {last}{parent}{archived}",
                    c.map(|c| c.active).unwrap_or(0),
                    c.map(|c| c.proposed).unwrap_or(0),
                );
            }
        }
        BranchCmd::Merge { src, into } => {
            let s = app.resolve_branch(Some(&src))?;
            let d = app.resolve_branch(Some(&into))?;
            let (merged, skipped) = app.merge(&s, &d)?;
            println!(
                "merged {merged} claim(s) from {s} into {d} ({skipped} skipped: already there or not held)"
            );
        }
        BranchCmd::Archive { name } => {
            let b = app.resolve_branch(Some(&name))?;
            let n = app.branch_archive(&b)?;
            println!("archived {b} ({n} claims flipped; nothing deleted)");
        }
    }
    Ok(())
}

/// ACE-style grow-and-refine without a model: surface near-duplicates so a
/// human can supersede one with the other.
fn refine(home: &CtxHome, branch: Option<String>, threshold: f64) -> Result<()> {
    use ctx_pack::select::{jaccard, word_set};
    let app = open(home)?;
    let b = app.resolve_branch(branch.as_deref())?;
    let claims = app.store.scan(&Filter {
        branch: Some(b.clone()),
        statuses: vec![Status::Active],
        ..Default::default()
    })?;
    let words: Vec<Vec<u64>> = claims.iter().map(|c| word_set(&c.text)).collect();
    let mut pairs = Vec::new();
    for i in 0..claims.len() {
        for j in i + 1..claims.len() {
            let s = jaccard(&words[i], &words[j]);
            if s >= threshold {
                pairs.push((s, i, j));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
    if pairs.is_empty() {
        println!("no near-duplicates on {b} at similarity ≥ {threshold}");
    }
    for (s, i, j) in pairs.iter().take(30) {
        let (older, newer) = (&claims[*i], &claims[*j]);
        println!("{:.0}% similar", s * 100.0);
        println!("  {} {}", tag(&older.cid), first_line(&older.text));
        println!("  {} {}", tag(&newer.cid), first_line(&newer.text));
        println!(
            "  merge: ctx save \"<combined>\" --supersedes {}   then: ctx archive {}",
            newer.id, older.id
        );
    }
    Ok(())
}

/// SessionStart hook: bounded pull, refresh AGENTS.md, and print the new
/// context only if it changed after the agent already loaded AGENTS.md.
/// Injecting only at session start keeps the prompt prefix stable, so
/// prompt caching keeps working (spec §12.2).
fn session_start(home: &CtxHome) -> Result<()> {
    if !home.exists() {
        return Ok(());
    }
    let mut app = open(home)?;
    if app.binding.is_none() {
        return Ok(());
    }
    if let Err(e) = app.pull_quick(Duration::from_millis(1500)) {
        eprintln!("ctx: pull skipped ({e})");
    }
    if let Ok(ctx_app::SpecFile::Written) = app.refresh_spec_file() {
        println!(
            "RecurOS: a new version of the spec was saved since your last session; .ctx/SPEC.md is updated. Re-read it before continuing.\n"
        );
    }
    if let Some(block) = app.refresh_agents_md()? {
        println!(
            "RecurOS: this project's context changed since AGENTS.md was loaded. Current version:\n\n{block}"
        );
    }
    Ok(())
}
