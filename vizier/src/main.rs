use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use serde::Serialize;
use vizier::diff::create_diff_envelope;
use vizier::observer::{ObserverConfig, WakeConfig, create_observer, create_waker};

#[derive(Debug, Parser)]
#[command(
    name = "vz",
    bin_name = "vz",
    version,
    about = "System perception utility"
)]
struct Cli {
    #[arg(long, global = true)]
    pretty: bool,

    #[arg(long, global = true)]
    verbose: bool,

    #[arg(long, global = true)]
    all_connections: bool,

    #[arg(long, global = true)]
    no_public_ip: bool,

    #[arg(long, global = true)]
    watch_path: Option<PathBuf>,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    Wake,
    Snapshot,
    Watch {
        #[arg(long, default_value_t = 1000)]
        interval: u64,

        #[arg(long)]
        diff: bool,
    },
}

fn main() {
    if let Err(err) = run() {
        eprintln!("{err:#}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    let cli = Cli::parse();
    let default_command = cli.command.is_none();
    let command = cli.command.unwrap_or(Command::Snapshot);

    match command {
        Command::Wake => {
            let waker = create_waker(WakeConfig {
                no_public_ip: cli.no_public_ip,
            });
            let wake = waker.wake()?;
            let wake = if cli.verbose { wake } else { wake.compact() };
            print_json(&wake, cli.pretty)?;
        }
        Command::Snapshot => {
            let mut observer = create_observer(ObserverConfig {
                watch_path: cli.watch_path,
                all_connections: cli.all_connections,
            });
            let snapshot = observer.snapshot()?;
            let snapshot = if default_command && !cli.verbose {
                snapshot.compact()
            } else {
                snapshot
            };
            print_json(&snapshot, cli.pretty || default_command)?;
        }
        Command::Watch { interval, diff } => {
            let mut observer = create_observer(ObserverConfig {
                watch_path: cli.watch_path,
                all_connections: cli.all_connections,
            });

            if diff {
                let mut previous = observer.snapshot()?;
                print_json(&previous, cli.pretty)?;

                loop {
                    thread::sleep(Duration::from_millis(interval));
                    let current = observer.snapshot()?;
                    let envelope = create_diff_envelope(&previous, &current)?;
                    print_json(&envelope, cli.pretty)?;
                    previous = current;
                }
            } else {
                loop {
                    let snapshot = observer.snapshot()?;
                    print_json(&snapshot, cli.pretty)?;
                    thread::sleep(Duration::from_millis(interval));
                }
            }
        }
    }

    Ok(())
}

fn print_json<T: Serialize>(value: &T, pretty: bool) -> Result<()> {
    let line = if pretty {
        serde_json::to_string_pretty(value)?
    } else {
        serde_json::to_string(value)?
    };

    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "{line}")?;
    stdout.flush()?;
    Ok(())
}
