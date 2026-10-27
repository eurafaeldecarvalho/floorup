//! Command line for `uv-bump`.

#![forbid(unsafe_code)]

use clap::{Parser, ValueEnum};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::atomic::{AtomicU64, Ordering};
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
        // JSON already carries the diff. A second copy would make stdout invalid JSON.
        if cli.dry_run && changed && cli.format == Format::Text {
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
        include_packages: tokens(&cli.packages),
        exclude_packages: tokens(&cli.exclude),
        include_groups: tokens(&cli.groups),
        exclude_groups: tokens(&cli.exclude_groups),
    }
}

fn tokens(values: &[String]) -> std::collections::BTreeSet<String> {
    values
        .iter()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .collect()
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

static TEMP_SEQ: AtomicU64 = AtomicU64::new(0);

fn write_atomic(path: &Path, contents: &str) -> Result<(), String> {
    // Replace the file that a symlink points at. Renaming over the symlink
    // itself would turn it into a regular file and leave the target stale.
    let dest = resolve_dest(path)?;
    let parent = dest
        .parent()
        .filter(|dir| !dir.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let file_name = dest
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "pyproject.toml".into());
    let perms = fs::metadata(&dest)
        .map_err(|err| format!("reading metadata of {}: {err}", dest.display()))?
        .permissions();

    for _ in 0..64 {
        let n = TEMP_SEQ.fetch_add(1, Ordering::Relaxed);
        let mut tmp_name = std::ffi::OsString::from(".");
        tmp_name.push(&file_name);
        tmp_name.push(format!(".uv-bump.{}.{n}.tmp", std::process::id()));
        let tmp = parent.join(tmp_name);
        // `create_new` is O_EXCL: an existing symlink is not followed.
        let file = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(file) => file,
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("writing {}: {err}", tmp.display())),
        };
        let wrote = (|| -> Result<(), String> {
            write_temp(file, &tmp, contents, &perms)?;
            fs::rename(&tmp, &dest)
                .map_err(|err| format!("replacing {}: {err}", dest.display()))?;
            Ok(())
        })();
        if let Err(err) = wrote {
            let _ = fs::remove_file(&tmp);
            return Err(err);
        }
        return Ok(());
    }
    Err(format!(
        "writing {}: could not create a temporary file",
        dest.display()
    ))
}

fn write_temp(
    mut file: fs::File,
    tmp: &Path,
    contents: &str,
    perms: &fs::Permissions,
) -> Result<(), String> {
    // Tighten permissions before the body is written so a 0600 pyproject is
    // never briefly world-readable, and fail rather than rename a looser file.
    file.set_permissions(perms.clone())
        .map_err(|err| format!("preserving permissions on {}: {err}", tmp.display()))?;
    file.write_all(contents.as_bytes())
        .map_err(|err| format!("writing {}: {err}", tmp.display()))?;
    file.sync_all()
        .map_err(|err| format!("writing {}: {err}", tmp.display()))?;
    Ok(())
}

fn resolve_dest(path: &Path) -> Result<PathBuf, String> {
    match fs::canonicalize(path) {
        Ok(dest) => Ok(dest),
        Err(err) => {
            if path.is_symlink() {
                Err(format!("resolving symlink {}: {err}", path.display()))
            } else {
                Ok(path.to_path_buf())
            }
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};

    #[test]
    fn atomic_write_keeps_mode_and_does_not_follow_symlinks() {
        let dir = std::env::temp_dir().join(format!("uv-bump-atomic-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let path = dir.join("pyproject.toml");
        fs::write(&path, "old\n").unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o640)).unwrap();
        write_atomic(&path, "new\n").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), "new\n");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o640
        );

        let real = dir.join("real.toml");
        fs::write(&real, "real\n").unwrap();
        fs::set_permissions(&real, fs::Permissions::from_mode(0o600)).unwrap();
        let link = dir.join("link.toml");
        symlink(&real, &link).unwrap();
        write_atomic(&link, "through\n").unwrap();
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(fs::read_to_string(&real).unwrap(), "through\n");
        assert_eq!(fs::read_to_string(&link).unwrap(), "through\n");
        assert_eq!(
            fs::metadata(&real).unwrap().permissions().mode() & 0o777,
            0o600
        );

        let secret = dir.join("secret");
        fs::write(&secret, "SECRET").unwrap();
        symlink(&secret, dir.join("pyproject.toml.uv-bump.tmp")).unwrap();
        let n = 50_000u64;
        TEMP_SEQ.store(n, Ordering::SeqCst);
        let planted = dir.join(format!(
            ".plant.toml.uv-bump.{}.{n}.tmp",
            std::process::id()
        ));
        symlink(&secret, &planted).unwrap();
        let plant = dir.join("plant.toml");
        fs::write(&plant, "plant\n").unwrap();
        fs::set_permissions(&plant, fs::Permissions::from_mode(0o604)).unwrap();
        write_atomic(&plant, "grown\n").unwrap();
        assert_eq!(fs::read_to_string(&secret).unwrap(), "SECRET");
        assert_eq!(fs::read_to_string(&plant).unwrap(), "grown\n");
        assert_eq!(
            fs::metadata(&plant).unwrap().permissions().mode() & 0o777,
            0o604
        );
        assert!(planted.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(dir
            .join("pyproject.toml.uv-bump.tmp")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        let names: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .filter(|name| name.ends_with(".tmp"))
            .collect();
        assert_eq!(
            names.len(),
            2,
            "only the symlinks planted by the test should remain: {names:?}"
        );

        let _ = fs::remove_dir_all(&dir);
    }
}
