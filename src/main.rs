use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};
use rrq::config::{grace_duration, Config, DEFAULT_REMOVE_GRACE_SECS};
use rrq::paths::Paths;
use rrq::requests::{self, RequestKind};
use rrq::state::State;
use rrq::{daemon, service, status};

#[derive(Parser)]
#[command(name = "rrq", version = concat!(env!("CARGO_PKG_VERSION"), " (", env!("RRQ_BUILD_SHA"), ")"), about = "Ready-to-use worktrees for the reviews requested from you, in Rex")]
struct Cli {
    #[command(subcommand)]
    cmd: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Runs the reconciler (used by launchd)
    Daemon,
    /// Shows the current state
    Status,
    /// Sends a request to the daemon
    Request {
        kind: Kind,
        /// PR as owner/repo#n
        #[arg(long)]
        pr: Option<String>,
    },
    /// Manages the LaunchAgent
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Interactive panel, in the current Rex block
    Tui,
    /// Installs the latest build of main (checks the SHA, verifies the checksum, restarts the daemon)
    Update {
        /// Only tells whether there is a newer build
        #[arg(long)]
        check: bool,
    },
}

fn load_cfg(p: &Paths) -> Result<Config> {
    Config::load(&p.config_file, &p.home)
}

#[derive(Clone, Copy, ValueEnum)]
enum Kind {
    Sync,
    Retry,
    Release,
    Adopt,
    ReleaseAll,
}

#[derive(Subcommand)]
enum ServiceAction {
    Install,
    Uninstall,
    Restart,
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    let paths = Paths::from_env()?;
    match cli.cmd {
        Command::Daemon => daemon::run(paths),
        Command::Status => {
            let state = State::read(&paths.state_file())?;
            let (grace, repos, poll) = match Config::load(&paths.config_file, &paths.home) {
                Ok(c) => (c.remove_grace_secs, c.repos.iter().map(|r| r.name.clone()).collect::<Vec<_>>(), c.poll_interval_secs),
                Err(_) => (DEFAULT_REMOVE_GRACE_SECS, Vec::new(), 60),
            };
            print!("{}", status::render_at(&state, chrono::Utc::now(), grace_duration(grace), &repos, poll));
            Ok(())
        }
        Command::Request { kind, pr } => {
            let kind = match kind {
                Kind::Sync => RequestKind::Sync,
                Kind::Retry => RequestKind::Retry,
                Kind::Release => RequestKind::Release,
                Kind::Adopt => RequestKind::Adopt,
                Kind::ReleaseAll => RequestKind::ReleaseAll,
            };
            let id = requests::write(&paths.requests_dir(), kind, pr)?;
            println!("request {id} sent; see the result with `rrq status`");
            Ok(())
        }
        Command::Service { action } => match action {
            ServiceAction::Install => service::install(&paths),
            ServiceAction::Uninstall => service::uninstall(&paths),
            ServiceAction::Restart => service::restart(),
        },
        Command::Tui => rrq::tui::run::run_logged(paths.clone(), || load_cfg(&paths)),
        Command::Update { check } => run_update(&paths, check),
    }
}

fn run_update(paths: &Paths, only_check: bool) -> Result<()> {
    use rrq::update::{self, Installed};
    let runner = rrq::runner::RealRunner;
    let local = update::local_sha();
    if only_check {
        match update::check(&runner)? {
            Some(sha) => println!("new build available: {} (running {})", update::short(&sha), update::short(local)),
            None => println!("up to date ({})", update::short(local)),
        }
        return Ok(());
    }
    let bin = paths.home.join(".local/bin/rrq");
    let work = paths.state_dir.join("update");
    match update::install(&runner, local, &bin, &work, &service::restart)? {
        Installed::UpToDate => println!("up to date ({})", update::short(local)),
        Installed::Replaced { sha } => {
            println!("rrq updated to {} and the daemon restarted", update::short(&sha));
            println!("an open panel keeps running the old version; quit it and run `rrq tui` again");
        }
    }
    Ok(())
}
