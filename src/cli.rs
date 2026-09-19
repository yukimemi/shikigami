//! コマンドライン。既定動作は TUI の起動。

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{CommandFactory, Parser, Subcommand};

use crate::app::App;
use crate::jj::Jj;
use crate::tui;

const CLI_STYLES: clap::builder::styling::Styles = {
    use clap::builder::styling::{AnsiColor, Effects, Styles};
    Styles::styled()
        .header(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::BrightBlue.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::Magenta.on_default())
        .error(AnsiColor::BrightRed.on_default().effects(Effects::BOLD))
        .valid(AnsiColor::BrightGreen.on_default())
        .invalid(AnsiColor::BrightYellow.on_default())
};

#[derive(Debug, Parser)]
#[command(
    author,
    version,
    about = "Terminal UI for Jujutsu (jj)",
    long_about = "shikigami drives the jj CLI: browse the change graph with its diffs, then run \
new / edit / describe / squash / rebase / abandon / undo from the log without leaving the TUI.",
    styles = CLI_STYLES
)]
pub struct Cli {
    /// Operate on this repo instead of the current directory
    #[arg(short = 'R', long, value_name = "PATH")]
    pub repository: Option<PathBuf>,

    /// Initial revset for the log pane (defaults to jj's own `revsets.log`)
    #[arg(short = 'r', long, value_name = "REVSET")]
    pub revisions: Option<String>,

    #[command(subcommand)]
    pub command: Option<Commands>,
}

#[derive(Debug, Subcommand)]
pub enum Commands {
    /// Print a shell completion script
    Completion {
        /// Shell to generate the script for
        #[arg(value_enum)]
        shell: clap_complete::Shell,
    },

    /// Update the shikigami binary itself to the latest GitHub release
    SelfUpdate {
        /// Skip the confirmation prompt
        #[arg(long, short = 'y')]
        yes: bool,

        /// Print availability and exit without installing
        #[arg(long)]
        check: bool,
    },
}

pub fn run() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::Completion { shell }) => {
            completion(shell);
            Ok(())
        }
        Some(Commands::SelfUpdate { yes, check }) => crate::update::run(yes, check),
        None => launch(cli.repository, cli.revisions),
    }
}

fn completion(shell: clap_complete::Shell) {
    let mut cmd = Cli::command();
    let bin = cmd.get_name().to_string();
    clap_complete::generate(shell, &mut cmd, bin, &mut std::io::stdout());
}

fn launch(repository: Option<PathBuf>, revisions: Option<String>) -> Result<()> {
    // jj が無い環境では TUI を開く前に落とす。alternate screen に入って
    // から「jj not found」を出すと、画面が戻る前に一瞬で消えて読めない。
    Jj::version().context("shikigami needs the `jj` binary on PATH")?;
    let start = match repository {
        Some(path) => path,
        None => std::env::current_dir().context("cannot determine the current directory")?,
    };
    let jj = Jj::discover(&start)?;
    let mut app = App::new(jj, revisions)?;
    tui::run(&mut app)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn completion_generates_for_every_supported_shell() {
        for shell in [
            clap_complete::Shell::Bash,
            clap_complete::Shell::Zsh,
            clap_complete::Shell::Fish,
            clap_complete::Shell::PowerShell,
            clap_complete::Shell::Elvish,
        ] {
            let mut cmd = Cli::command();
            let mut out = Vec::new();
            clap_complete::generate(shell, &mut cmd, "shikigami", &mut out);
            assert!(!out.is_empty(), "{shell} completion was empty");
        }
    }

    #[test]
    fn repository_and_revset_flags_parse_in_short_form() {
        // shikigami.nvim はこの 2 つの短縮形しか渡さない。壊すと
        // plugin 側が無言で別の repo を開くことになる。
        let cli = Cli::try_parse_from(["shikigami", "-R", "/tmp/repo", "-r", "all()"]).unwrap();
        assert_eq!(cli.repository.unwrap(), PathBuf::from("/tmp/repo"));
        assert_eq!(cli.revisions.unwrap(), "all()");
        assert!(cli.command.is_none());
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }
}
