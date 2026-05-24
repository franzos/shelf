use std::io::Write;
use std::process::ExitCode;

use clap::Parser;
use shelf::cli::{Cli, Command, HealthArgs, RevertArgs, RunArgs, RunsArgs, VerifyArgs};
use shelf::config::{default_config_dir, discover_profiles};
use shelf::error::Error;
use shelf::health::DEFAULT_SAMPLE;
use shelf::run::{health, revert_run, run, runs_list, runs_show, status, verify};
use tracing_subscriber::EnvFilter;

const EXIT_STRICT_FAIL: u8 = 3;
const EXIT_HEALTH_FOUND: u8 = 4;

fn main() -> ExitCode {
    let cli = Cli::parse();
    init_tracing(cli.verbose);

    let strict = cli.strict;
    let verbose = cli.verbose;

    match dispatch(&cli) {
        Ok(DispatchOutcome::Ok) => ExitCode::SUCCESS,
        Ok(DispatchOutcome::HealthFlagged) if strict => {
            let _ = writeln!(
                std::io::stderr(),
                "strict: health entries surfaced; failing with exit {EXIT_STRICT_FAIL}"
            );
            ExitCode::from(EXIT_STRICT_FAIL)
        }
        Ok(DispatchOutcome::HealthFlagged) => ExitCode::SUCCESS,
        Ok(DispatchOutcome::HealthSubcommandFlagged) => ExitCode::from(EXIT_HEALTH_FOUND),
        Err(err) => {
            print_error(&err, verbose);
            match err {
                Error::Unimplemented(_)
                | Error::ProfileNotFound { .. }
                | Error::ProfileAmbiguous { .. }
                | Error::NoProfiles { .. }
                | Error::NoConfigDir
                | Error::Validation { .. } => ExitCode::from(2),
                _ => ExitCode::from(1),
            }
        }
    }
}

#[derive(Debug, Clone, Copy)]
enum DispatchOutcome {
    Ok,
    /// At least one health entry surfaced during `run`/`plan`. Maps to
    /// exit 3 under `--strict`, 0 otherwise.
    HealthFlagged,
    /// `shelf health` or `shelf verify` reported one or more entries.
    /// Maps to exit 4 regardless of `--strict`.
    HealthSubcommandFlagged,
}

fn dispatch(cli: &Cli) -> shelf::error::Result<DispatchOutcome> {
    match &cli.command {
        Command::List => {
            cmd_list()?;
            Ok(DispatchOutcome::Ok)
        }
        Command::Run(args) => cmd_run(cli, args, cli.dry_run),
        Command::Plan(args) => cmd_run(cli, args, true),
        Command::Status => {
            cmd_status(cli)?;
            Ok(DispatchOutcome::Ok)
        }
        Command::Health(args) => cmd_health(cli, args),
        Command::Verify(args) => cmd_verify(cli, args),
        Command::Runs(args) => {
            cmd_runs(cli, args)?;
            Ok(DispatchOutcome::Ok)
        }
        Command::Revert(args) => {
            cmd_revert(cli, args)?;
            Ok(DispatchOutcome::Ok)
        }
    }
}

fn cmd_list() -> shelf::error::Result<()> {
    let dir = default_config_dir()?;
    let entries = discover_profiles(&dir)?;
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for e in entries {
        let _ = writeln!(out, "{}\t{}", e.name, e.path.display());
    }
    Ok(())
}

fn cmd_run(cli: &Cli, args: &RunArgs, dry_run: bool) -> shelf::error::Result<DispatchOutcome> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut err = std::io::stderr();
    let outcome = run(
        args.profile.as_deref(),
        cli.config.as_deref(),
        &args.from,
        dry_run,
        cli.strict,
        args.all,
        &mut out,
        &mut err,
    )?;
    if outcome.has_health_entries() {
        Ok(DispatchOutcome::HealthFlagged)
    } else {
        Ok(DispatchOutcome::Ok)
    }
}

fn cmd_health(cli: &Cli, args: &HealthArgs) -> shelf::error::Result<DispatchOutcome> {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let sample = args.sample.unwrap_or(DEFAULT_SAMPLE);
    let outcome = health(
        args.profile.as_deref(),
        cli.config.as_deref(),
        sample,
        &mut out,
        &mut err,
    )?;
    if outcome.has_entries() {
        Ok(DispatchOutcome::HealthSubcommandFlagged)
    } else {
        Ok(DispatchOutcome::Ok)
    }
}

fn cmd_verify(cli: &Cli, args: &VerifyArgs) -> shelf::error::Result<DispatchOutcome> {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let outcome = verify(
        args.profile.as_deref(),
        cli.config.as_deref(),
        args.full,
        args.sample,
        &mut out,
        &mut err,
    )?;
    if outcome.has_entries() {
        Ok(DispatchOutcome::HealthSubcommandFlagged)
    } else {
        Ok(DispatchOutcome::Ok)
    }
}

fn cmd_status(cli: &Cli) -> shelf::error::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    status(cli.config.as_deref(), &mut out)
}

fn cmd_runs(cli: &Cli, args: &RunsArgs) -> shelf::error::Result<()> {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let (profile, id) = resolve_runs_args(args);
    match id {
        Some(id) => runs_show(profile.as_deref(), cli.config.as_deref(), id, &mut out),
        None => runs_list(
            profile.as_deref(),
            cli.config.as_deref(),
            args.limit,
            &mut out,
        ),
    }
}

fn cmd_revert(cli: &Cli, args: &RevertArgs) -> shelf::error::Result<()> {
    let stdout = std::io::stdout();
    let stderr = std::io::stderr();
    let mut out = stdout.lock();
    let mut err = stderr.lock();
    let (profile, id) = resolve_revert_args(args)?;
    revert_run(
        profile.as_deref(),
        cli.config.as_deref(),
        id,
        cli.dry_run,
        args.force,
        &mut out,
        &mut err,
    )?;
    Ok(())
}

/// Disambiguate the optional positional args to `shelf runs`:
/// - `(None, None)` → list, default profile
/// - `(Some("photos"), None)` → list, profile = photos
/// - `(Some("photos"), Some(42))` → show, profile = photos, id = 42
/// - `(Some("42"), None)` and `42` parses as i64 → show, default profile, id = 42
fn resolve_runs_args(args: &RunsArgs) -> (Option<String>, Option<i64>) {
    match (args.profile_or_id.as_deref(), args.id) {
        (Some(first), Some(id)) => (Some(first.to_string()), Some(id)),
        (Some(first), None) => match first.parse::<i64>() {
            Ok(id) if id > 0 => (None, Some(id)),
            _ => (Some(first.to_string()), None),
        },
        (None, _) => (None, None),
    }
}

/// Disambiguate the positional args to `shelf revert`. When the second is
/// present, the first is the profile name; otherwise the first is parsed
/// as an id.
fn resolve_revert_args(args: &RevertArgs) -> shelf::error::Result<(Option<String>, i64)> {
    match args.id {
        Some(id) => Ok((Some(args.profile_or_id.clone()), id)),
        None => match args.profile_or_id.parse::<i64>() {
            Ok(id) if id > 0 => Ok((None, id)),
            _ => Err(Error::RevertRefused(format!(
                "expected a numeric run id, got `{}`",
                args.profile_or_id,
            ))),
        },
    }
}

/// Print the error message to stderr. Under `-vv` walk the source chain.
fn print_error(err: &Error, verbose: u8) {
    let _ = writeln!(std::io::stderr(), "error: {err}");
    if verbose >= 2 {
        let mut source: Option<&dyn std::error::Error> = std::error::Error::source(err);
        while let Some(s) = source {
            let _ = writeln!(std::io::stderr(), "  caused by: {s}");
            source = s.source();
        }
    }
}

/// Configure the global tracing subscriber. `RUST_LOG` wins over the
/// verbosity flag when set explicitly.
fn init_tracing(verbosity: u8) {
    let default_level = match verbosity {
        0 => "warn",
        1 => "info",
        2 => "debug",
        _ => "trace",
    };
    let filter =
        EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_level));
    tracing_subscriber::fmt().with_env_filter(filter).init();
}
