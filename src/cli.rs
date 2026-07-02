use std::path::PathBuf;

use anyhow::{Result, anyhow};
use clap::{ArgGroup, Parser, ValueEnum};
use regex::Regex;

#[derive(Parser)]
#[command(version, about)]
#[command(group(
    ArgGroup::new("mode")
        .args(["bench", "bin"])
        .required(true)
))]
pub struct Cli {
    #[arg(short = 'p', long = "package")]
    pub package: String,
    #[arg(long)]
    pub bench: Option<String>,
    #[arg(long)]
    pub bin: Option<String>,
    #[arg(long = "args", requires = "bin")]
    pub args: Option<String>,
    #[arg(last = true, requires = "bin")]
    pub trailing_args: Vec<String>,
    #[arg(long = "rev")]
    pub rev: String,
    #[arg(long = "rev-base", default_value = "HEAD")]
    pub rev_base: String,
    #[arg(long = "reps", default_value_t = 5, value_parser = clap::value_parser!(u32).range(1..))]
    pub reps: u32,
    #[arg(long = "metric-regex", requires = "bin")]
    pub metric_regex: Option<String>,
    #[arg(long = "metric-dir", default_value = "higher")]
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
    #[arg(long = "work-dir")]
    pub work_dir: Option<PathBuf>,
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
}
