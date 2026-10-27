//! Command line for `uv-bump`.

#![forbid(unsafe_code)]

use clap::{Parser, ValueEnum};
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};
use uv_bump::{
    bump_str, default_cache_dir, unified, Options, Outcome, PypiConfig, PypiIndex, VersionSource,
};

#[derive(Debug, Parser)]
#[command(
    name = "uv-bump",
    version,
    about = "Bump dependency bounds in pyproject.toml to the latest PyPI releases.",
    after_help = "\
Exact pins (==, ===) and upper caps (<, <=) are left in place. >=, >, and ~= move \
forward. Pass --bump-pins to rewrite exact pins, and --dry-run to print a diff."
)]
struct Cli {
    /// Path to pyproject.toml, or a directory containing one.
    #[arg(value_name = "PYPROJECT", default_value = "pyproject.toml")]
    path: PathBuf,

    /// Print a unified diff and do not write the file.
    #[arg(short = 'n', long)]
    dry_run: bool,

    /// Exit 1 when any bound would change. Does not write the file.
    #[arg(long)]
    check: bool,

    /// Consider pre-releases and development releases.
    #[arg(long)]
    pre: bool,

    /// Rewrite exact `==` pins to the latest release. `===` is never rewritten.
    #[arg(long)]
    bump_pins: bool,

    /// Let `~=` track the absolute latest release instead of its current series.
    #[arg(long)]
    expand_compatible: bool,

    /// Only bump these packages. Repeat the flag or separate names with commas.
    #[arg(short = 'p', long = "package", value_delimiter = ',')]
    packages: Vec<String>,

    /// Skip these packages.
    #[arg(long = "exclude", value_delimiter = ',')]
    exclude: Vec<String>,

    /// Only bump these groups (`project`, `dev`, an extra name, a dependency group, `build-system`).
    #[arg(long = "group", value_delimiter = ',')]
    groups: Vec<String>,

    /// Skip these groups.
    #[arg(long = "exclude-group", value_delimiter = ',')]
    exclude_groups: Vec<String>,

    /// Run `uv lock` after a successful write. The pyproject.toml is restored if locking fails.
    #[arg(long)]
    lock: bool,

    /// PyPI JSON API root. A URL ending in `/simple` is rewritten to the sibling `/pypi` root.
    #[arg(long, default_value = "https://pypi.org/pypi")]
    index_url: String,

    /// Do not read or write the on-disk response cache.
    #[arg(long)]
    no_cache: bool,

    /// Use only the on-disk cache.
    #[arg(long)]
    offline: bool,

    /// Directory for cached PyPI responses.
    #[arg(long)]
    cache_dir: Option<PathBuf>,

    /// Cache lifetime in seconds. 0 stores responses but never reuses them.
    #[arg(long, default_value_t = 3600)]
    cache_ttl: u64,

    /// Concurrent package lookups.
    #[arg(long, default_value_t = 16)]
    jobs: usize,

    /// HTTP timeout in seconds for each lookup.
    #[arg(long, default_value_t = 30)]
    timeout: u64,

    /// Continue after a lookup failure. Packages that could not be looked up are left unchanged.
    #[arg(long)]
    keep_going: bool,

    /// List pins, missing packages, and other skips.
    #[arg(short = 'v', long)]
    verbose: bool,

    /// Suppress the summary. A dry-run diff is still printed.
    #[arg(short = 'q', long)]
    quiet: bool,

    /// `text` prints a summary. `json` prints one object.
    #[arg(long, value_enum, default_value_t = Format::Text)]
    format: Format,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
enum Format {
    Text,
    Json,
}

fn main() -> ExitCode {
    match run() {
        Ok(true) => ExitCode::SUCCESS,
        Ok(false) => ExitCode::from(1),
        Err(err) => {
            eprintln!("uv-bump: {err}");
            ExitCode::from(2)
        }
    }
}

fn run() -> Result<bool, String> {
    let cli = Cli::parse();
    if cli.lock && (cli.dry_run || cli.check) {
        return Err(
            "--lock writes pyproject.toml and cannot be combined with --dry-run or --check"
                .to_string(),
        );
    }
    let path = resolve_path(&cli.path)?;
    let original =
        fs::read_to_string(&path).map_err(|err| format!("reading {}: {err}", path.display()))?;
    let opts = options_from(&cli);
    let index = PypiIndex::new(PypiConfig {
        index_url: cli.index_url.clone(),
        cache_dir: if cli.no_cache {
            None
        } else {
            cli.cache_dir.clone().or_else(default_cache_dir)
        },
        ttl: Duration::from_secs(cli.cache_ttl),
        timeout: Duration::from_secs(cli.timeout),
        jobs: cli.jobs,
        offline: cli.offline,
        keep_going: cli.keep_going,
    })
    .map_err(|err| err.to_string())?;

    let started = Instant::now();
    let outcome =
        bump_str(&original, &index as &dyn VersionSource, &opts).map_err(|err| err.to_string())?;
    let elapsed = started.elapsed();
    let changed = outcome.updated != original;

    let diff = unified(&display_path(&path), &original, &outcome.updated);
    if cli.format == Format::Json {
        write_json(&path, &outcome, elapsed, &diff)?;
    } else if !cli.quiet {
        let mut target: Box<dyn Write> = if cli.dry_run {
            Box::new(io::stderr())
        } else {
            Box::new(io::stdout())
        };
        let _ = write!(target, "{}", summary(&outcome, elapsed));
        if cli.verbose {
            for note in &outcome.notes {
                let _ = writeln!(
                    target,
                    "  {}: {} ({}) [{}]",
                    note.requirement, note.detail, note.group, note.reason
                );
            }
        }
    }

    if cli.dry_run || cli.check {
        if cli.dry_run && changed {
            print!("{diff}");
        }
        return Ok(!cli.check || !changed);
    }

    if changed {
        write_atomic(&path, &outcome.updated)?;
        if cli.lock {
            if let Err(err) = uv_lock(path.parent().unwrap_or_else(|| Path::new("."))) {
                write_atomic(&path, &original)?;
                return Err(format!("{err}; restored {}", path.display()));
            }
        }
    }
    Ok(true)
}

fn options_from(cli: &Cli) -> Options {
    Options {
        allow_pre: cli.pre,
        bump_pins: cli.bump_pins,
        expand_compatible: cli.expand_compatible,
        include_packages: cli.packages.iter().cloned().collect(),
        exclude_packages: cli.exclude.iter().cloned().collect(),
        include_groups: cli.groups.iter().cloned().collect(),
        exclude_groups: cli.exclude_groups.iter().cloned().collect(),
    }
}

fn resolve_path(path: &Path) -> Result<PathBuf, String> {
    if path.is_dir() {
        Ok(path.join("pyproject.toml"))
    } else {
        Ok(path.to_path_buf())
    }
}

fn display_path(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

fn summary(outcome: &Outcome, elapsed: Duration) -> String {
    let count = outcome.changes.len();
    let noun = if count == 1 {
        "dependency"
    } else {
        "dependencies"
    };
    let mut line = format!("bumped {count} {noun} in {:.3}s", elapsed.as_secs_f64());
    let pins = outcome
        .notes
        .iter()
        .filter(|note| note.reason == "exact_pin")
        .count();
    if pins > 0 {
        line.push_str(&format!("; {pins} exact pins unchanged"));
    }
    if outcome.already_current > 0 {
        line.push_str(&format!("; {} already current", outcome.already_current));
    }
    if outcome.unbounded > 0 {
        line.push_str(&format!("; {} without a version bound", outcome.unbounded));
    }
    let other = outcome
        .notes
        .iter()
        .filter(|note| note.reason != "exact_pin")
        .count();
    if other > 0 {
        line.push_str(&format!("; {other} skipped"));
    }
    line.push('\n');
    let mut body = line;
    for change in &outcome.changes {
        body.push_str(&format!(
            "{}  ->  {}  ({})\n",
            change.before, change.after, change.group
        ));
    }
    body
}

fn write_json(path: &Path, outcome: &Outcome, elapsed: Duration, diff: &str) -> Result<(), String> {
    let value = serde_json::json!({
        "path": path.display().to_string(),
        "bumped": outcome.changes.len(),
        "already_current": outcome.already_current,
        "unbounded": outcome.unbounded,
        "elapsed_ms": elapsed.as_millis(),
        "changes": outcome.changes,
        "notes": outcome.notes,
        "diff": diff,
    });
    serde_json::to_writer_pretty(io::stdout(), &value).map_err(|err| err.to_string())?;
    println!();
    Ok(())
}

fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    let parent = path
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let mut tmp_name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "pyproject.toml".into());
    tmp_name.push(".uv-bump.tmp");
    let tmp = parent.join(tmp_name);
    fs::write(&tmp, contents).map_err(|err| format!("writing {}: {err}", tmp.display()))?;
    if let Ok(meta) = fs::metadata(path) {
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).map_err(|err| format!("replacing {}: {err}", path.display()))?;
    Ok(())
}

fn uv_lock(project_dir: &Path) -> Result<(), String> {
    let status = Command::new("uv")
        .arg("lock")
        .current_dir(project_dir)
        .status()
        .map_err(|err| {
            if err.kind() == io::ErrorKind::NotFound {
                "uv was not found on PATH; install uv or drop --lock".to_string()
            } else {
                format!("failed to run uv lock: {err}")
            }
        })?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("uv lock exited with {status}"))
    }
}
