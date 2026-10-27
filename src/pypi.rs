//! PyPI JSON API client, on-disk cache, and in-memory version maps for tests.

use crate::parse::normalize_name;
use crate::Error;
use pep440_rs::Version;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_BODY: usize = 25 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Release {
    pub version_text: String,
    pub version: Version,
    pub yanked: bool,
}

impl Release {
    pub fn parse(text: &str) -> Option<Self> {
        let version = text.parse().ok()?;
        Some(Self {
            version_text: text.to_string(),
            version,
            yanked: false,
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum LookupError {
    #[error("package was not found on the index")]
    NotFound,
    #[error("{0}")]
    Failed(String),
}

pub trait VersionSource: Send + Sync {
    fn releases(&self, normalized_name: &str) -> Result<Vec<Release>, LookupError>;

    /// Warm the cache for `names`. The default implementation does nothing.
    fn prefetch(&self, names: &[String]) -> Result<(), Error> {
        let _ = names;
        Ok(())
    }
}

/// Fixed map of normalized package name to releases. Unknown names are [`LookupError::NotFound`].
#[derive(Debug, Default, Clone)]
pub struct MapIndex {
    map: BTreeMap<String, Vec<Release>>,
}

impl MapIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: &str, versions: &[&str]) {
        let releases = versions.iter().filter_map(|v| Release::parse(v)).collect();
        self.map.insert(normalize_name(name), releases);
    }

    pub fn insert_releases(&mut self, name: &str, releases: Vec<Release>) {
        self.map.insert(normalize_name(name), releases);
    }
}

impl VersionSource for MapIndex {
    fn releases(&self, normalized_name: &str) -> Result<Vec<Release>, LookupError> {
        self.map
            .get(normalized_name)
            .cloned()
            .ok_or(LookupError::NotFound)
    }
}

#[derive(Debug, Clone)]
pub struct PypiConfig {
    pub index_url: String,
    pub cache_dir: Option<PathBuf>,
    pub ttl: Duration,
    pub timeout: Duration,
    pub jobs: usize,
    pub offline: bool,
    pub keep_going: bool,
}

impl Default for PypiConfig {
    fn default() -> Self {
        Self {
            index_url: "https://pypi.org/pypi".to_string(),
            cache_dir: default_cache_dir(),
            ttl: Duration::from_secs(3600),
            timeout: Duration::from_secs(30),
            jobs: 16,
            offline: false,
            keep_going: false,
        }
    }
}

pub struct PypiIndex {
    client: reqwest::blocking::Client,
    index_url: String,
    cache_dir: Option<PathBuf>,
    ttl: Duration,
    offline: bool,
    keep_going: bool,
    jobs: usize,
    mem: Mutex<HashMap<String, Result<Vec<Release>, LookupError>>>,
}

impl PypiIndex {
    pub fn new(config: PypiConfig) -> Result<Self, Error> {
        let client = reqwest::blocking::Client::builder()
            .user_agent(format!(
                "floorup/{} (+https://github.com/eurafaeldecarvalho/floorup)",
                env!("CARGO_PKG_VERSION")
            ))
            .timeout(config.timeout)
            .redirect(reqwest::redirect::Policy::limited(10))
            .gzip(true)
            .build()
            .map_err(|e| Error::Message(format!("failed to build HTTP client: {e}")))?;
        Ok(Self {
            client,
            index_url: config.index_url,
            cache_dir: config.cache_dir,
            ttl: config.ttl,
            offline: config.offline,
            keep_going: config.keep_going,
            jobs: config.jobs.max(1),
            mem: Mutex::new(HashMap::new()),
        })
    }
}

impl VersionSource for PypiIndex {
    fn releases(&self, normalized_name: &str) -> Result<Vec<Release>, LookupError> {
        if let Some(hit) = self.mem_get(normalized_name) {
            return hit;
        }
        let result = self.fetch_one(normalized_name);
        self.mem_insert(normalized_name, result.clone());
        result
    }

    fn prefetch(&self, names: &[String]) -> Result<(), Error> {
        if names.is_empty() {
            return Ok(());
        }
        let jobs = self.jobs.min(names.len());
        let next = Mutex::new(0usize);
        let failures: Mutex<Vec<String>> = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            for _ in 0..jobs {
                scope.spawn(|| loop {
                    let idx = {
                        let mut cursor = lock_mutex(&next);
                        if *cursor >= names.len() {
                            return;
                        }
                        let idx = *cursor;
                        *cursor += 1;
                        idx
                    };
                    let name = &names[idx];
                    if self.mem_get(name).is_some() {
                        continue;
                    }
                    let result = self.fetch_one(name);
                    if let Err(LookupError::Failed(msg)) = &result {
                        lock_mutex(&failures).push(format!("{name}: {msg}"));
                    }
                    self.mem_insert(name, result);
                });
            }
        });
        let failures = lock_mutex(&failures).clone();
        if !failures.is_empty() && !self.keep_going {
            return Err(Error::Message(format!(
                "version lookup failed: {}",
                failures.join("; ")
            )));
        }
        Ok(())
    }
}

impl PypiIndex {
    fn mem_get(&self, name: &str) -> Option<Result<Vec<Release>, LookupError>> {
        lock_mutex(&self.mem).get(name).cloned()
    }

    fn mem_insert(&self, name: &str, result: Result<Vec<Release>, LookupError>) {
        lock_mutex(&self.mem).insert(name.to_string(), result);
    }

    fn fetch_one(&self, name: &str) -> Result<Vec<Release>, LookupError> {
        if let Some(hit) = self.read_cache(name) {
            return hit;
        }
        if self.offline {
            return Err(LookupError::Failed(format!(
                "{name} is not in the cache and --offline is set"
            )));
        }
        let url = json_endpoint(&self.index_url, name);
        let response = self
            .client
            .get(&url)
            .send()
            .map_err(|e| LookupError::Failed(format!("GET {} failed: {e}", redact(&url))))?;
        let status = response.status();
        if status.as_u16() == 404 {
            self.write_cache(name, &CacheEntry::missing());
            return Err(LookupError::NotFound);
        }
        if !status.is_success() {
            return Err(LookupError::Failed(format!(
                "GET {} returned {status}",
                redact(&url)
            )));
        }
        let bytes = response
            .bytes()
            .map_err(|e| LookupError::Failed(format!("GET {} failed: {e}", redact(&url))))?;
        if bytes.len() > MAX_BODY {
            return Err(LookupError::Failed(format!(
                "response for {name} exceeded {MAX_BODY} bytes"
            )));
        }
        let text = String::from_utf8(bytes.to_vec())
            .map_err(|_| LookupError::Failed(format!("response for {name} was not UTF-8")))?;
        let releases = parse_pypi_json(&text)
            .map_err(|e| LookupError::Failed(format!("invalid JSON for {name}: {e}")))?;
        self.write_cache(name, &CacheEntry::from_releases(&releases));
        Ok(releases)
    }

    fn read_cache(&self, name: &str) -> Option<Result<Vec<Release>, LookupError>> {
        let path = cache_path(self.cache_dir.as_deref()?, &self.index_url, name);
        let raw = fs::read_to_string(path).ok()?;
        let entry: CacheEntry = serde_json::from_str(&raw).ok()?;
        let now = unix_now();
        if self.ttl.is_zero() || now.saturating_sub(entry.fetched_unix) > self.ttl.as_secs() {
            return None;
        }
        if !entry.found {
            return Some(Err(LookupError::NotFound));
        }
        Some(Ok(entry.into_releases()))
    }

    fn write_cache(&self, name: &str, entry: &CacheEntry) {
        let Some(dir) = self.cache_dir.as_deref() else {
            return;
        };
        let path = cache_path(dir, &self.index_url, name);
        if let Some(parent) = path.parent() {
            if fs::create_dir_all(parent).is_err() {
                return;
            }
        }
        let Ok(body) = serde_json::to_vec(entry) else {
            return;
        };
        // A predictable `name.json.tmp` is followed by `fs::write` when an
        // attacker plants a symlink. Create a unique file with O_EXCL instead.
        let file_name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or(name);
        for _ in 0..64 {
            let n = CACHE_SEQ.fetch_add(1, Ordering::Relaxed);
            let tmp = path.with_file_name(format!(".{file_name}.{}.{n}.tmp", std::process::id()));
            let mut file = match OpenOptions::new().write(true).create_new(true).open(&tmp) {
                Ok(file) => file,
                Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(_) => return,
            };
            if file.write_all(&body).is_err() {
                let _ = fs::remove_file(&tmp);
                return;
            }
            if fs::rename(&tmp, &path).is_err() {
                let _ = fs::remove_file(&tmp);
                return;
            }
            return;
        }
    }
}

static CACHE_SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug, Serialize, Deserialize)]
struct CacheEntry {
    fetched_unix: u64,
    found: bool,
    releases: Vec<CachedRelease>,
}

#[derive(Debug, Serialize, Deserialize)]
struct CachedRelease {
    version: String,
    yanked: bool,
}

impl CacheEntry {
    fn missing() -> Self {
        Self {
            fetched_unix: unix_now(),
            found: false,
            releases: Vec::new(),
        }
    }

    fn from_releases(releases: &[Release]) -> Self {
        Self {
            fetched_unix: unix_now(),
            found: true,
            releases: releases
                .iter()
                .map(|rel| CachedRelease {
                    version: rel.version_text.clone(),
                    yanked: rel.yanked,
                })
                .collect(),
        }
    }

    fn into_releases(self) -> Vec<Release> {
        self.releases
            .into_iter()
            .filter_map(|rel| {
                let mut parsed = Release::parse(&rel.version)?;
                parsed.yanked = rel.yanked;
                Some(parsed)
            })
            .collect()
    }
}

#[derive(Deserialize)]
struct PypiFile {
    #[serde(default)]
    yanked: bool,
}

#[derive(Deserialize)]
struct PypiBody {
    releases: BTreeMap<String, Vec<PypiFile>>,
}

pub fn parse_pypi_json(body: &str) -> Result<Vec<Release>, String> {
    let parsed: PypiBody = serde_json::from_str(body).map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    for (version_text, files) in parsed.releases {
        if files.is_empty() {
            continue;
        }
        let Some(mut release) = Release::parse(&version_text) else {
            continue;
        };
        release.yanked = files.iter().all(|file| file.yanked);
        out.push(release);
    }
    Ok(out)
}

pub fn json_endpoint(index: &str, name: &str) -> String {
    let trimmed = index.trim();
    let cut = trimmed.find(['?', '#']).unwrap_or(trimmed.len());
    let (path, suffix) = trimmed.split_at(cut);
    let mut base = path.trim_end_matches('/').to_string();
    if let Some(stripped) = base.strip_suffix("/simple") {
        base = format!("{stripped}/pypi");
    }
    format!("{base}/{}/json{suffix}", path_segment(name))
}

fn path_segment(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

pub fn redact(url: &str) -> String {
    let Some(scheme_end) = url.find("://") else {
        return url.to_string();
    };
    let rest = &url[scheme_end + 3..];
    // Userinfo is the part of the authority before the last `@`. A password
    // may itself contain `@`; the first `@` would leak the rest of it.
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    let Some(at) = authority.rfind('@') else {
        return url.to_string();
    };
    format!(
        "{}://***@{}{}",
        &url[..scheme_end],
        &authority[at + 1..],
        &rest[auth_end..]
    )
}

pub fn default_cache_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("FLOORUP_CACHE_DIR") {
        return Some(PathBuf::from(dir));
    }
    if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg).join("floorup"));
    }
    std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").join("floorup"))
}

fn cache_path(dir: &Path, index: &str, name: &str) -> PathBuf {
    dir.join(format!("{:016x}", fnv64(index.as_bytes())))
        .join(format!("{name}.json"))
}

fn fnv64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn lock_mutex<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_rewrites_simple_index_and_redacts_userinfo() {
        assert_eq!(
            json_endpoint("https://pypi.org/pypi", "requests"),
            "https://pypi.org/pypi/requests/json"
        );
        assert_eq!(
            json_endpoint("https://pypi.org/simple/", "requests"),
            "https://pypi.org/pypi/requests/json"
        );
        assert_eq!(
            redact("https://user:token@example.com/pypi/requests/json"),
            "https://***@example.com/pypi/requests/json"
        );
        assert_eq!(
            redact("https://user:p@ss@example.com/simple/pkg/json"),
            "https://***@example.com/simple/pkg/json"
        );
        assert_eq!(
            json_endpoint("https://example.com/simple?token=a", "requests"),
            "https://example.com/pypi/requests/json?token=a"
        );
        assert_eq!(
            redact("https://example.com/pypi/pkg@2/json"),
            "https://example.com/pypi/pkg@2/json"
        );
    }

    #[test]
    fn json_skips_empty_unparsed_and_marks_yanked() {
        let body = r#"{
            "releases": {
                "1.0.0": [{"yanked": false}],
                "2.0.0": [{"yanked": true}, {"yanked": true}],
                "2.1.0": [{"yanked": true}, {"yanked": false}],
                "3.0.0": [],
                "nope": [{"yanked": false}],
                "2.0.0rc1": [{"yanked": false}]
            }
        }"#;
        let releases = parse_pypi_json(body).unwrap();
        let by_name = |v: &str| {
            releases
                .iter()
                .find(|rel| rel.version_text == v)
                .unwrap_or_else(|| panic!("missing {v}"))
        };
        assert!(!by_name("1.0.0").yanked);
        assert!(by_name("2.0.0").yanked);
        assert!(!by_name("2.1.0").yanked);
        assert!(!by_name("2.0.0rc1").yanked);
        assert!(releases.iter().all(|rel| rel.version_text != "3.0.0"));
        assert!(releases.iter().all(|rel| rel.version_text != "nope"));
    }

    #[test]
    fn cache_write_does_not_follow_a_planted_temp_symlink() {
        use std::os::unix::fs::symlink;

        let dir =
            std::env::temp_dir().join(format!("floorup-cache-symlink-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let index = PypiIndex::new(PypiConfig {
            index_url: "https://pypi.org/pypi".into(),
            cache_dir: Some(dir.clone()),
            ttl: Duration::from_secs(3600),
            timeout: Duration::from_secs(5),
            jobs: 1,
            offline: true,
            keep_going: false,
        })
        .unwrap();
        let path = cache_path(&dir, "https://pypi.org/pypi", "requests");
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let secret = dir.join("secret");
        fs::write(&secret, "SECRET").unwrap();
        symlink(&secret, path.with_extension("json.tmp")).unwrap();
        CACHE_SEQ.store(7, Ordering::SeqCst);
        let planted = path.with_file_name(format!(".requests.json.{}.7.tmp", std::process::id()));
        symlink(&secret, &planted).unwrap();

        let entry = CacheEntry::from_releases(&[Release::parse("2.32.3").unwrap()]);
        index.write_cache("requests", &entry);

        assert_eq!(fs::read_to_string(&secret).unwrap(), "SECRET");
        assert!(planted.symlink_metadata().unwrap().file_type().is_symlink());
        assert!(path
            .with_extension("json.tmp")
            .symlink_metadata()
            .unwrap()
            .file_type()
            .is_symlink());
        let cached = fs::read_to_string(&path).unwrap();
        assert!(cached.contains("2.32.3"), "{cached}");
        assert!(!path.symlink_metadata().unwrap().file_type().is_symlink());
    }

    #[test]
    fn map_index_normalizes_names() {
        let mut map = MapIndex::new();
        map.insert("Typing_Extensions", &["4.0.0"]);
        let found = map.releases("typing-extensions").unwrap();
        assert_eq!(found[0].version_text, "4.0.0");
        assert!(matches!(
            map.releases("missing"),
            Err(LookupError::NotFound)
        ));
    }
}
