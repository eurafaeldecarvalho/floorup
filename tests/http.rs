//! Mocked PyPI JSON server. CI runs this; it does not talk to the network.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use uv_bump::{bump_str, Options, PypiConfig, PypiIndex, VersionSource};

struct Server {
    url: String,
    hits: Arc<AtomicUsize>,
    shutdown: Arc<AtomicBool>,
    port: u16,
    handle: Option<std::thread::JoinHandle<()>>,
}

impl Server {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let hits = Arc::new(AtomicUsize::new(0));
        let shutdown = Arc::new(AtomicBool::new(false));
        let hits_t = Arc::clone(&hits);
        let shutdown_t = Arc::clone(&shutdown);
        let handle = std::thread::spawn(move || {
            while !shutdown_t.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((sock, _)) => {
                        if shutdown_t.load(Ordering::SeqCst) {
                            break;
                        }
                        hits_t.fetch_add(1, Ordering::SeqCst);
                        let _ = respond(sock);
                    }
                    Err(_) => break,
                }
            }
        });
        Self {
            url: format!("http://127.0.0.1:{port}/pypi"),
            hits,
            shutdown,
            port,
            handle: Some(handle),
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(("127.0.0.1", self.port));
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn respond(mut sock: TcpStream) -> std::io::Result<()> {
    sock.set_read_timeout(Some(Duration::from_secs(2)))?;
    let mut buf = Vec::new();
    let mut byte = [0u8; 1];
    while buf.len() < 65_536 {
        match sock.read(&mut byte) {
            Ok(0) => break,
            Ok(_) => {
                buf.push(byte[0]);
                if buf.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    let text = String::from_utf8_lossy(&buf);
    let path = text
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("");
    let name = path.trim_start_matches("/pypi/").trim_end_matches("/json");
    let (status, body) = match name {
        "requests" => (
            200,
            r#"{"releases":{"2.28.0":[{"yanked":false}],"2.32.3":[{"yanked":false}],"2.33.0rc1":[{"yanked":false}]}}"#,
        ),
        "yankedpkg" => (
            200,
            r#"{"releases":{"1.0.0":[{"yanked":false}],"9.0.0":[{"yanked":true}]}}"#,
        ),
        "boom" => (500, r#"{"message":"nope"}"#),
        "missing" => (404, r#"{"message":"not found"}"#),
        _ => (404, r#"{"message":"not found"}"#),
    };
    let raw = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(raw.as_bytes())?;
    Ok(())
}

fn config(server: &Server, cache: Option<PathBuf>, offline: bool, keep_going: bool) -> PypiConfig {
    PypiConfig {
        index_url: server.url.clone(),
        cache_dir: cache,
        ttl: Duration::from_secs(3600),
        timeout: Duration::from_secs(5),
        jobs: 4,
        offline,
        keep_going,
    }
}

fn temp_dir(label: &str) -> PathBuf {
    static N: AtomicUsize = AtomicUsize::new(0);
    let dir = std::env::temp_dir().join(format!(
        "uv-bump-{label}-{}-{}",
        std::process::id(),
        N.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn client_selects_latest_stable_and_caches() {
    let server = Server::start();
    let cache = temp_dir("cache");
    let index = PypiIndex::new(config(&server, Some(cache.clone()), false, false)).unwrap();
    index
        .prefetch(&["requests".into(), "yankedpkg".into()])
        .unwrap();
    assert_eq!(server.hits(), 2);
    let requests = index.releases("requests").unwrap();
    assert!(requests.iter().any(|rel| rel.version_text == "2.32.3"));
    let text =
        "[project]\nname='t'\nversion='0'\ndependencies=['requests>=2.0','yankedpkg>=0.1']\n";
    let out = bump_str(text, &index, &Options::default()).unwrap();
    assert!(out.updated.contains("requests>=2.32.3"));
    assert!(out.updated.contains("yankedpkg>=1.0.0"));
    assert!(!out.updated.contains("2.33.0rc1"));
    assert!(!out.updated.contains("9.0.0"));

    let again = PypiIndex::new(config(&server, Some(cache), false, false)).unwrap();
    let cached = again.releases("requests").unwrap();
    assert_eq!(server.hits(), 2, "warm cache should not hit the server");
    assert!(cached.iter().any(|rel| rel.version_text == "2.32.3"));
}

#[test]
fn offline_uses_cache_and_fails_when_empty() {
    let server = Server::start();
    let cache = temp_dir("offline");
    let index = PypiIndex::new(config(&server, Some(cache.clone()), false, false)).unwrap();
    index.prefetch(&["requests".into()]).unwrap();
    let hits = server.hits();
    let offline = PypiIndex::new(config(&server, Some(cache.clone()), true, false)).unwrap();
    assert!(offline.releases("requests").is_ok());
    assert_eq!(server.hits(), hits);
    let empty = PypiIndex::new(config(&server, Some(temp_dir("empty")), true, false)).unwrap();
    let err = empty.prefetch(&["requests".into()]).unwrap_err();
    assert!(err.to_string().contains("offline"), "{err}");
}

#[test]
fn hard_failure_stops_unless_keep_going() {
    let server = Server::start();
    let index = PypiIndex::new(config(&server, None, false, false)).unwrap();
    let err = index
        .prefetch(&["requests".into(), "boom".into()])
        .unwrap_err();
    assert!(err.to_string().contains("boom"), "{err}");

    let index = PypiIndex::new(config(&server, None, false, true)).unwrap();
    index.prefetch(&["requests".into(), "boom".into()]).unwrap();
    let text = "[project]\nname='t'\nversion='0'\ndependencies=['requests>=1','boom>=1']\n";
    let out = bump_str(text, &index, &Options::default()).unwrap();
    assert!(out.updated.contains("requests>=2.32.3"));
    assert!(out.updated.contains("boom>=1"));
    assert!(out.notes.iter().any(|note| note.reason == "lookup_failed"));
}

#[test]
fn cli_dry_run_check_and_write() {
    let server = Server::start();
    let dir = temp_dir("cli");
    let path = dir.join("pyproject.toml");
    let original =
        "[project]\nname='t'\nversion='0'\ndependencies=['requests>=2.0.0','missing>=1']\n";
    std::fs::write(&path, original).unwrap();
    let bin = env!("CARGO_BIN_EXE_uv-bump");

    let dry = Command::new(bin)
        .arg(path.as_os_str())
        .arg("--dry-run")
        .arg("--index-url")
        .arg(&server.url)
        .arg("--no-cache")
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let diff = String::from_utf8_lossy(&dry.stdout);
    assert!(
        diff.contains("-") && diff.contains("requests>=2.32.3"),
        "{diff}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

    let check = Command::new(bin)
        .arg(&path)
        .arg("--check")
        .arg("--quiet")
        .arg("--index-url")
        .arg(&server.url)
        .arg("--no-cache")
        .status()
        .unwrap();
    assert_eq!(check.code(), Some(1));

    let write = Command::new(bin)
        .arg(&path)
        .arg("--index-url")
        .arg(&server.url)
        .arg("--no-cache")
        .arg("--format")
        .arg("json")
        .output()
        .unwrap();
    assert!(
        write.status.success(),
        "{}",
        String::from_utf8_lossy(&write.stderr)
    );
    let updated = std::fs::read_to_string(&path).unwrap();
    assert!(updated.contains("requests>=2.32.3"));
    assert!(updated.contains("missing>=1"));
    let parsed: serde_json::Value = serde_json::from_slice(&write.stdout).unwrap();
    assert_eq!(parsed["bumped"], 1);

    let check_clean = Command::new(bin)
        .arg(&path)
        .arg("--check")
        .arg("--index-url")
        .arg(&server.url)
        .arg("--no-cache")
        .status()
        .unwrap();
    assert!(check_clean.success());

    let excluded = temp_dir("exclude");
    let excluded_path = excluded.join("pyproject.toml");
    std::fs::write(&excluded_path, original).unwrap();
    let status = Command::new(bin)
        .arg(&excluded_path)
        .arg("--exclude")
        .arg("requests")
        .arg("--index-url")
        .arg(&server.url)
        .arg("--no-cache")
        .status()
        .unwrap();
    assert!(status.success());
    assert_eq!(std::fs::read_to_string(&excluded_path).unwrap(), original);
}

#[test]
fn json_dry_run_is_one_document_and_lookup_failure_does_not_write() {
    let server = Server::start();
    let dir = temp_dir("json");
    let path = dir.join("pyproject.toml");
    let original = "[project]\nname='t'\nversion='0'\ndependencies=['requests>=1']\n";
    std::fs::write(&path, original).unwrap();
    let bin = env!("CARGO_BIN_EXE_uv-bump");

    let dry = Command::new(bin)
        .arg(&path)
        .arg("--dry-run")
        .arg("--format")
        .arg("json")
        .arg("--index-url")
        .arg(&server.url)
        .arg("--no-cache")
        .arg("--package")
        .arg(" requests ,")
        .output()
        .unwrap();
    assert!(
        dry.status.success(),
        "{}",
        String::from_utf8_lossy(&dry.stderr)
    );
    let stdout = String::from_utf8(dry.stdout).unwrap();
    let parsed: serde_json::Value = serde_json::from_str(stdout.trim())
        .unwrap_or_else(|err| panic!("stdout was not one JSON value: {err}\n{stdout}"));
    assert!(
        parsed["diff"].as_str().unwrap().contains("requests>="),
        "{parsed}"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);

    let bad = dir.join("bad.toml");
    let bad_original = "[project]\nname='t'\nversion='0'\ndependencies=['requests>=1','boom>=1']\n";
    std::fs::write(&bad, bad_original).unwrap();
    let failed = Command::new(bin)
        .arg(&bad)
        .arg("--index-url")
        .arg(&server.url)
        .arg("--no-cache")
        .output()
        .unwrap();
    assert_eq!(failed.status.code(), Some(2), "{failed:?}");
    assert_eq!(std::fs::read_to_string(&bad).unwrap(), bad_original);
}
