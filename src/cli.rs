use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{ArgGroup, CommandFactory, Parser, Subcommand, ValueEnum};
use regex::Regex;

#[derive(Parser)]
#[command(version, about)]
#[command(subcommand_negates_reqs = true, args_conflicts_with_subcommands = true)]
#[command(group(
    ArgGroup::new("mode")
        .args(["bench", "bin"])
        .required(true)
))]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Sub>,
    #[arg(short = 'p', long = "package")]
    pub package: Option<String>,
    #[arg(long)]
    pub bench: Option<String>,
    #[arg(long)]
    pub bin: Option<String>,
    #[arg(long = "args", requires = "bin")]
    pub args: Option<String>,
    #[arg(last = true, requires = "bin")]
    pub trailing_args: Vec<String>,
    #[arg(long = "rev")]
    pub rev: Option<String>,
    #[arg(long = "rev-base", default_value = "HEAD")]
    pub rev_base: String,
    #[arg(long = "reps", value_parser = clap::value_parser!(u32).range(1..))]
    pub reps: Option<u32>,
    #[arg(long = "metric-regex", requires = "bin")]
    pub metric_regex: Option<String>,
    #[arg(
        long = "metric-dir",
        default_value = "higher",
        requires = "metric_regex"
    )]
    pub metric_dir: MetricDir,
    #[arg(long = "runs-on-core", default_value_t = 0)]
    pub runs_on_core: u32,
    #[arg(long = "no-pin")]
    pub no_pin: bool,
    #[arg(long = "profile", default_value = "release-tuned")]
    pub profile: String,
    #[arg(long = "json")]
    pub json: bool,
    #[arg(long = "keep-worktrees")]
    pub keep_worktrees: bool,
    #[arg(long = "work-dir", value_hint = clap::ValueHint::DirPath)]
    pub work_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
pub enum Sub {
    /// Generate shell completions
    Completions(CompletionsArgs),
    /// Print completion candidates (used by shell completion scripts)
    #[command(name = "__candidates", hide = true)]
    Candidates { kind: CandidateKind },
}

#[derive(clap::Args)]
pub struct CompletionsArgs {
    /// bash | zsh | fish | elvish | powershell | nushell
    pub shell: CompletionShell,
    /// Write the script to the conventional location instead of stdout
    #[arg(long)]
    pub install: bool,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum CandidateKind {
    Packages,
    Bins,
    Benches,
    Revs,
    Profiles,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum CompletionShell {
    Bash,
    Zsh,
    Fish,
    Elvish,
    Powershell,
    Nushell,
}

#[derive(Clone, Copy, ValueEnum)]
pub enum MetricDir {
    Higher,
    Lower,
}

pub enum Mode {
    Criterion {
        bench: String,
    },
    Binary {
        bin: String,
        args: Vec<String>,
        metric: MetricSource,
    },
}

pub enum MetricSource {
    WallClock,
    Regex {
        raw: String,
        pattern: Regex,
        higher_is_better: bool,
    },
}

impl Cli {
    pub fn parse_from_env() -> Self {
        let mut args: Vec<String> = std::env::args().collect();
        if args.get(1).map(String::as_str) == Some("bench-compare") {
            args.remove(1);
        }
        Self::parse_from(args)
    }

    pub fn mode(&self) -> Result<Mode> {
        if let Some(bench) = &self.bench {
            return Ok(Mode::Criterion {
                bench: bench.clone(),
            });
        }

        let bin = self
            .bin
            .clone()
            .ok_or_else(|| anyhow!("exactly one of --bench/--bin is required"))?;
        let mut args = Vec::new();
        if let Some(raw) = &self.args {
            args.extend(raw.split_ascii_whitespace().map(str::to_owned));
        }
        args.extend(self.trailing_args.clone());

        let metric = if let Some(raw) = &self.metric_regex {
            let pattern = Regex::new(raw)?;
            if pattern.captures_len() < 2 {
                return Err(anyhow!("--metric-regex must contain a capture group"));
            }
            MetricSource::Regex {
                raw: raw.clone(),
                pattern,
                higher_is_better: matches!(self.metric_dir, MetricDir::Higher),
            }
        } else {
            MetricSource::WallClock
        };

        Ok(Mode::Binary { bin, args, metric })
    }

    pub fn package(&self) -> Result<&str> {
        self.package
            .as_deref()
            .ok_or_else(|| anyhow!("--package <PACKAGE> is required"))
    }

    pub fn rev(&self) -> Result<&str> {
        self.rev
            .as_deref()
            .ok_or_else(|| anyhow!("--rev <REV> is required"))
    }
}

pub fn command() -> clap::Command {
    <Cli as CommandFactory>::command()
}
