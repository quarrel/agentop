mod model;
mod rollout;
mod schema;
mod schema_sync;
mod ui;

use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    version,
    about = "Read-only live observer for Codex multi-agent rollouts"
)]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,
    #[arg(long)]
    sessions_dir: Option<PathBuf>,
    #[arg(long)]
    session: Option<String>,
    /// Colour output mode
    #[arg(long, value_enum, default_value_t = ui::ColorMode::Auto)]
    color: ui::ColorMode,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Fetch and catalogue RolloutLine schemas from new official Codex releases
    BuildSchema {
        /// Catalogue directory (defaults to AGENTOP_SCHEMA_DIR or the XDG data directory)
        #[arg(long)]
        catalogue_dir: Option<PathBuf>,
    },
}

fn sessions_dir(explicit: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = explicit {
        return Ok(path);
    }
    if let Some(home) = std::env::var_os("CODEX_HOME") {
        return Ok(PathBuf::from(home).join("sessions"));
    }
    let home = std::env::var_os("HOME").context("HOME is not set")?;
    Ok(PathBuf::from(home).join(".codex/sessions"))
}

fn require_terminal() -> Result<()> {
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("interactive terminal required: run agentop directly in a TTY (input or output is redirected)");
    }
    Ok(())
}

fn open_selected_reader(
    discovery: rollout::Discovery,
    requested: &str,
    dir: PathBuf,
    catalogue_dir: PathBuf,
) -> Result<rollout::SelectedReader> {
    let rollout::Discovery {
        admitted,
        pending,
        health: _archive_health,
    } = discovery;
    let groups = rollout::group(admitted);
    let selected = rollout::select(&groups, Some(requested))?.clone();
    rollout::SelectedReader::new(selected, pending, dir, catalogue_dir)
}

fn run() -> Result<()> {
    let args = Args::parse();
    if let Some(Command::BuildSchema { catalogue_dir }) = args.command {
        let catalogue_dir = catalogue_dir.map_or_else(schema::default_catalogue_dir, Ok)?;
        let report = schema_sync::build_schema(&catalogue_dir)?;
        if report.imported.is_empty() {
            println!(
                "Schema catalogue is current: {} official tags mapped",
                report.official_tags
            );
        } else {
            for item in &report.imported {
                println!(
                    "{} -> {}{}",
                    item.version,
                    item.rollout_line_sha256,
                    if item.new_family { " (new family)" } else { "" }
                );
            }
            println!(
                "Imported {} versions from {} official tags into {}",
                report.imported.len(),
                report.official_tags,
                catalogue_dir.display()
            );
        }
        return Ok(());
    }

    let dir = sessions_dir(args.sessions_dir)?;
    let catalogue_dir = schema::default_catalogue_dir()?;
    require_terminal()?;
    if let Some(requested) = args.session.as_deref() {
        let discovery = rollout::discover(&dir)
            .with_context(|| format!("discover sessions under {}", dir.display()))?;
        let mut reader = open_selected_reader(discovery, requested, dir, catalogue_dir)?;
        ui::run(&mut reader, args.color)
    } else {
        ui::run_browser(dir, catalogue_dir, args.color)
    }
}

fn main() {
    if let Err(error) = run() {
        eprintln!("agentop: {error:#}");
        std::process::exit(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::DiagnosticSample;
    use clap::CommandFactory;
    use std::fs;

    #[test]
    fn color_cli_accepts_exact_values_and_defaults_to_auto() {
        let default = Args::try_parse_from(["agentop"]).unwrap();
        assert!(default.command.is_none());
        assert_eq!(default.color, ui::ColorMode::Auto);
        assert_eq!(
            Args::try_parse_from(["agentop", "--color=auto"])
                .unwrap()
                .color,
            ui::ColorMode::Auto
        );
        assert_eq!(
            Args::try_parse_from(["agentop", "--color=none"])
                .unwrap()
                .color,
            ui::ColorMode::None
        );
        assert!(Args::try_parse_from(["agentop", "--color=always"]).is_err());
        assert!(matches!(
            Args::try_parse_from([
                "agentop",
                "build-schema",
                "--catalogue-dir",
                "/tmp/catalogue"
            ])
            .unwrap()
            .command,
            Some(Command::BuildSchema {
                catalogue_dir: Some(_)
            })
        ));
        let mut help = Vec::new();
        Args::command().write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("--color <COLOR>"));
        assert!(help.contains("[possible values: none, auto]"));
        assert_eq!(
            Args::command().get_version(),
            Some(env!("CARGO_PKG_VERSION"))
        );
        let version = Args::try_parse_from(["agentop", "--version"])
            .err()
            .expect("--version should exit after displaying the version");
        assert_eq!(version.kind(), clap::error::ErrorKind::DisplayVersion);
        assert!(version
            .to_string()
            .contains(concat!("agentop ", env!("CARGO_PKG_VERSION"))));
    }
    #[test]
    fn selected_reader_excludes_unrelated_archive_health() {
        let temp = tempfile::tempdir().expect("temporary sessions directory");
        let path = temp.path().join("rollout-test.jsonl");
        fs::write(
            &path,
            b"{\"type\":\"session_meta\",\"payload\":{\"session_id\":\"session\",\"id\":\"root\",\"cli_version\":\"0.152.1\",\"cwd\":\"/repo\"}}\n",
        )
        .expect("write rollout fixture");

        let mut discovery = rollout::discover(temp.path()).expect("discover fixture");
        discovery.health.unknown_records = 7;
        discovery.health.diagnostic(DiagnosticSample {
            rollout_path: temp.path().join("unrelated.jsonl"),
            byte_offset: 11,
            cli_version: None,
            kind: "unrelated_archive_diagnostic".into(),
            ordinal: None,
            detail: None,
        });

        let reader = open_selected_reader(
            discovery,
            "session",
            temp.path().to_path_buf(),
            PathBuf::from("/repo"),
        )
        .expect("open selected reader");

        assert_eq!(reader.state.data_health.unknown_records, 0);
        assert!(reader.state.data_health.recent_diagnostics.is_empty());
    }
}
