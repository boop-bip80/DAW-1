//! Scanning in helper processes: each `.clap` is loaded by a short-lived child (the app's own
//! executable started with [`SCAN_FLAG`]) that prints its descriptors and exits.
//!
//! Scanning in-process loads every installed binary for the life of the process, with whatever
//! its static initialisers take (thread-specific data keys among them, of which a process has
//! only `PTHREAD_KEYS_MAX`). See `soundcraft-vst3-host`'s `isolate` module, which does the same
//! for VST3: out of process, the app only loads the plugins it uses, and a plugin that crashes
//! or hangs while being scanned is skipped instead of taking the app down.

use crate::{ClapDescriptor, ClapError, parse_id, registry};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, PoisonError, mpsc};
use std::time::{Duration, Instant};

/// The argument that makes an executable scan one bundle (the path follows) and exit.
pub const SCAN_FLAG: &str = "--scan-clap-bundle";
/// The line a child prints after its result; anything else is a crash.
const END: &str = "soundcraft-clap-scan-end";
/// Longest a child may take to load one bundle.
const TIMEOUT: Duration = Duration::from_secs(60);
/// Longest a child may take to exit once it has printed its result (module exits).
const EXIT_GRACE: Duration = Duration::from_secs(5);
/// Upper bound on what is read from one child.
const MAX_OUTPUT: u64 = 8 << 20;
/// Most children running at once.
const MAX_WORKERS: usize = 8;

fn scanner() -> &'static Mutex<Option<PathBuf>> {
    static S: OnceLock<Mutex<Option<PathBuf>>> = OnceLock::new();
    S.get_or_init(|| Mutex::new(None))
}

/// Sets (or clears) the executable that scans bundles in child processes.
pub fn set_scanner(exe: Option<PathBuf>) {
    *scanner().lock().unwrap_or_else(PoisonError::into_inner) = exe;
}

/// The executable that scans bundles, when one is set.
pub fn scanner_exe() -> Option<PathBuf> {
    scanner().lock().unwrap_or_else(PoisonError::into_inner).clone()
}

/// The child side: loads `path` in this process and prints the result. Returns the process exit
/// code.
pub fn child_main(path: &Path) -> i32 {
    let result = registry::load_bundle(path).map_err(|e| e.to_string());
    let code = i32::from(result.is_err());
    let json = serde_json::to_string(&result).unwrap_or_else(|e| format!("{{\"Err\":\"cannot encode the scan: {e}\"}}"));
    let mut out = std::io::stdout().lock();
    let _ = writeln!(out, "{json}\n{END}");
    let _ = out.flush();
    code
}

/// Decodes a child's output for `path`: descriptors with valid ids, all pointing at `path`.
pub(crate) fn decode(path: &Path, json: &str) -> Result<Vec<ClapDescriptor>, ClapError> {
    let err = |m: String| ClapError::Load(path.display().to_string(), m);
    let result: Result<Vec<ClapDescriptor>, String> = serde_json::from_str(json).map_err(|e| err(format!("unreadable scan result: {e}")))?;
    let mut ds = result.map_err(|m| err(m.strip_prefix(&format!("cannot load CLAP plugin {}: ", path.display())).unwrap_or(&m).to_string()))?;
    ds.retain(|d| parse_id(&d.id) == Some(d.plugin_id.as_str()));
    for d in &mut ds {
        d.path = path.to_path_buf();
    }
    Ok(ds)
}

/// Kills `child` unless it exits within `grace`, and reaps it.
fn reap(mut child: Child, grace: Duration) -> Option<std::process::ExitStatus> {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Some(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                return child.wait().ok();
            }
        }
    }
}

/// Scans one bundle in a child process.
pub(crate) fn scan_one(exe: &Path, path: &Path) -> Result<Vec<ClapDescriptor>, ClapError> {
    let err = |m: String| ClapError::Load(path.display().to_string(), m);
    let mut child = Command::new(exe)
        .arg(SCAN_FLAG)
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| err(format!("cannot start the plugin scanner: {e}")))?;
    let Some(stdout) = child.stdout.take() else {
        reap(child, Duration::ZERO);
        return Err(err("the plugin scanner has no output".into()));
    };
    // Read on a thread so a child that never writes (or leaves a helper process holding the
    // pipe open) cannot block past the timeout.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let mut json = None;
        for line in BufReader::new(stdout.take(MAX_OUTPUT)).lines() {
            let Ok(line) = line else { break };
            if line == END {
                break;
            }
            json = Some(line);
        }
        let _ = tx.send(json);
    });
    match rx.recv_timeout(TIMEOUT) {
        Ok(Some(json)) => {
            reap(child, EXIT_GRACE);
            decode(path, &json)
        }
        Ok(None) | Err(mpsc::RecvTimeoutError::Disconnected) => {
            let status = reap(child, EXIT_GRACE);
            Err(err(format!("the plugin crashed while being scanned ({})", status.map_or_else(|| "no status".to_string(), |s| s.to_string()))))
        }
        Err(mpsc::RecvTimeoutError::Timeout) => {
            reap(child, Duration::ZERO);
            Err(err(format!("the plugin did not load within {} s", TIMEOUT.as_secs())))
        }
    }
}

/// Scans `bundles` in child processes, several at a time. Results are in the order of `bundles`.
pub(crate) fn scan_all(exe: &Path, bundles: &[PathBuf]) -> Vec<Result<Vec<ClapDescriptor>, ClapError>> {
    let workers = std::thread::available_parallelism().map_or(2, |n| n.get()).clamp(1, MAX_WORKERS).min(bundles.len());
    let next = AtomicUsize::new(0);
    let done: Mutex<Vec<(usize, Result<Vec<ClapDescriptor>, ClapError>)>> = Mutex::new(Vec::with_capacity(bundles.len()));
    std::thread::scope(|s| {
        for _ in 0..workers {
            s.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::Relaxed);
                    let Some(path) = bundles.get(i) else { break };
                    let r = scan_one(exe, path);
                    done.lock().unwrap_or_else(PoisonError::into_inner).push((i, r));
                }
            });
        }
    });
    let mut done = done.into_inner().unwrap_or_else(PoisonError::into_inner);
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_dsp::Category;

    fn desc(plugin_id: &str) -> ClapDescriptor {
        ClapDescriptor {
            id: format!("{}{plugin_id}", crate::ID_PREFIX),
            plugin_id: plugin_id.to_string(),
            name: "Comp".into(),
            vendor: "V".into(),
            version: "1".into(),
            description: String::new(),
            features: vec!["audio-effect".into(), "compressor".into()],
            category: Category::Dynamics,
            is_instrument: false,
            path: PathBuf::from("/elsewhere.clap"),
        }
    }

    #[test]
    fn child_output_decodes_and_is_not_trusted() {
        let path = Path::new("/p/Comp.clap");
        let good = desc("com.v.comp");
        let mut bad = desc("com.v.comp");
        bad.id = "clap:com.v.other".into();
        let json = serde_json::to_string(&Ok::<_, String>(vec![good.clone(), bad])).unwrap();
        let ds = decode(path, &json).unwrap();
        assert_eq!(ds.len(), 1, "invalid ids are dropped");
        assert_eq!(ds[0].path, path, "the path is the one the parent asked for");
        assert_eq!(ds[0].name, good.name);

        let json =
            serde_json::to_string(&Err::<Vec<ClapDescriptor>, _>(format!("cannot load CLAP plugin {}: no loadable binary", path.display()))).unwrap();
        assert_eq!(decode(path, &json).unwrap_err().to_string(), format!("cannot load CLAP plugin {}: no loadable binary", path.display()));
        assert!(decode(path, "not json").is_err());
        assert!(decode(path, "").is_err());
    }

    #[test]
    fn a_scanner_that_cannot_start_is_an_error() {
        let r = scan_one(Path::new("/no/such/scanner"), Path::new("/p/x.clap"));
        assert!(r.unwrap_err().to_string().contains("cannot start the plugin scanner"));
    }

    // Unix only: uses `/bin/sh` scripts as stand-in scanners.
    #[cfg(unix)]
    #[test]
    fn crashing_and_silent_scanners_are_errors_and_results_keep_their_order() {
        use std::os::unix::fs::PermissionsExt;
        let d = std::env::temp_dir().join(format!("soundcraft-clap-isolate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let script = |name: &str, body: &str| {
            let p = d.join(name);
            std::fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
            std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
            p
        };
        let crash = script("crash.sh", "kill -ABRT $$");
        let r = scan_one(&crash, Path::new("/p/x.clap"));
        assert!(r.unwrap_err().to_string().contains("crashed"));

        let json = serde_json::to_string(&Ok::<_, String>(vec![desc("com.v.comp")])).unwrap();
        // Prints a result for bundles named `ok*`, crashes on the others.
        let mixed = script("mixed.sh", &format!("case \"$2\" in */ok*) printf '%s\\n{END}\\n' '{json}';; *) exit 3;; esac"));
        let bundles: Vec<PathBuf> =
            (0..12).map(|i| PathBuf::from(if i % 3 == 0 { format!("/p/bad{i}.clap") } else { format!("/p/ok{i}.clap") })).collect();
        let results = scan_all(&mixed, &bundles);
        assert_eq!(results.len(), bundles.len());
        for (b, r) in bundles.iter().zip(&results) {
            match r {
                Ok(ds) => {
                    assert!(b.to_string_lossy().contains("/ok"));
                    assert_eq!(ds[0].path, *b);
                }
                Err(_) => assert!(b.to_string_lossy().contains("/bad")),
            }
        }
        let _ = std::fs::remove_dir_all(&d);
    }
}
