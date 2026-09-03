mod model;
mod rollout;
mod schema;
mod ui;

use anyhow::{bail, Context, Result};
use clap::Parser;
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(about = "Read-only live observer for Codex multi-agent rollouts")]
struct Args {
    #[arg(long)]
    sessions_dir: Option<PathBuf>,
    #[arg(long)]
    session: Option<String>,
    /// Colour output mode
    #[arg(long, value_enum, default_value_t = ui::ColorMode::Auto)]
    color: ui::ColorMode,
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
    cwd: PathBuf,
) -> Result<rollout::SelectedReader> {
    let rollout::Discovery {
        admitted,
        pending,
        health: _archive_health,
    } = discovery;
    let groups = rollout::group(admitted);
    let selected = rollout::select(&groups, Some(requested))?.clone();
    rollout::SelectedReader::new(selected, pending, dir, cwd)
}

fn run() -> Result<()> {
    let args = Args::parse();
    let dir = sessions_dir(args.sessions_dir)?;
    let cwd = std::env::current_dir().context("resolve current working directory")?;

    require_terminal()?;
    if let Some(requested) = args.session.as_deref() {
        let discovery = rollout::discover(&dir)
            .with_context(|| format!("discover sessions under {}", dir.display()))?;
        let mut reader = open_selected_reader(discovery, requested, dir, cwd)?;
        ui::run(&mut reader, args.color)
    } else {
        ui::run_browser(dir, cwd, args.color)
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

        let mut help = Vec::new();
        Args::command().write_long_help(&mut help).unwrap();
        let help = String::from_utf8(help).unwrap();
        assert!(help.contains("--color <COLOR>"));
        assert!(help.contains("[possible values: none, auto]"));
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
