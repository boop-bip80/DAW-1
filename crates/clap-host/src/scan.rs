//! Finding CLAP plugins on disk (safe, pure `std::fs`) and mapping CLAP features to categories.

use soundcraft_dsp::Category;
use std::path::{Component, Path, PathBuf};

/// Directory recursion limit while scanning (CLAP folders may be nested, never deeply).
const MAX_DEPTH: usize = 8;
/// Upper bound on bundles collected by one scan (hostile or huge folders).
const MAX_BUNDLES: usize = 4096;
/// Upper bound on directory entries visited by one scan.
const MAX_ENTRIES: usize = 100_000;

/// Set (to anything but `0` or empty) to scan only the `CLAP_PATH` / `VST3_PATH` folders and skip
/// the standard locations: a test harness then loads only the plugins it names.
pub const PATHS_ONLY_ENV: &str = "SOUNDCRAFT_PLUGIN_PATHS_ONLY";
/// A path list (like `PATH`) of plugins and folders never loaded; see [`is_blocked`].
pub const BLOCKLIST_ENV: &str = "SOUNDCRAFT_PLUGIN_BLOCKLIST";

/// The standard CLAP search locations for this OS (existing or not), followed by every entry
/// of the `CLAP_PATH` environment variable. With [`PATHS_ONLY_ENV`] set, only the `CLAP_PATH` entries.
pub fn default_search_paths() -> Vec<PathBuf> {
    let os = os_search_paths(
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("COMMONPROGRAMFILES").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
    );
    search_paths(os, std::env::var_os("CLAP_PATH").as_deref(), env_flag(PATHS_ONLY_ENV))
}

/// The standard locations followed by the entries of a path-list variable, or only those entries
/// when `only_env` is set.
pub fn search_paths(standard: Vec<PathBuf>, env: Option<&std::ffi::OsStr>, only_env: bool) -> Vec<PathBuf> {
    let mut v = if only_env { Vec::new() } else { standard };
    if let Some(cp) = env {
        v.extend(std::env::split_paths(cp).filter(|p| !p.as_os_str().is_empty()));
    }
    v
}

/// True when an environment flag is set to anything but empty or `0`.
pub fn env_flag(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty() && v != "0")
}

/// The entries of [`BLOCKLIST_ENV`] (empty when unset).
pub fn blocklist_from_env() -> Vec<PathBuf> {
    let Some(v) = std::env::var_os(BLOCKLIST_ENV) else { return Vec::new() };
    std::env::split_paths(&v)
        .filter(|p| !p.as_os_str().is_empty())
        .filter(|p| match entry_problem(p) {
            Some(why) => {
                log::warn!("{BLOCKLIST_ENV}: ignoring `{}`: {why}", p.display());
                false
            }
            None => true,
        })
        .collect()
}

/// Why a blocklist entry or rescan target can't be used (`None` when it can). An entry is either a
/// bare bundle name (one plain component) or an absolute path below the root; relative paths
/// with folders, `..`, and a bare root would match unpredictably or block everything.
pub fn entry_problem(e: &Path) -> Option<&'static str> {
    let normal = e.components().filter(|c| matches!(c, Component::Normal(_))).count();
    if e.components().any(|c| c == Component::ParentDir) {
        Some("must not contain `..`")
    } else if !e.is_absolute() && !(normal == 1 && e.components().count() == 1) {
        Some("must be an absolute path or a bare bundle name")
    } else if e.is_absolute() && normal == 0 {
        Some("must name a folder or bundle below the root")
    } else {
        None
    }
}

/// A path as comparable components: `.` dropped, names lower-cased (plugin folders usually live
/// on case-insensitive file systems; on case-sensitive ones this can only block more).
fn folded(p: &Path) -> Vec<String> {
    p.components().filter(|c| *c != Component::CurDir).map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect()
}

/// True when `path` is `root` or lies inside it, compared by whole components.
pub fn is_under(path: &Path, root: &Path) -> bool {
    let r = folded(root);
    !r.is_empty() && folded(path).starts_with(&r)
}

/// `path` with its links resolved, without the `\\?\` prefix Windows adds (so it compares with
/// the paths people type); `None` when it doesn't exist.
fn resolved(path: &Path) -> Option<PathBuf> {
    let r = std::fs::canonicalize(path).ok()?;
    let text = r.to_string_lossy();
    Some(match text.strip_prefix(r"\\?\UNC\") {
        Some(rest) => PathBuf::from(format!(r"\\{rest}")),
        None => text.strip_prefix(r"\\?\").map_or_else(|| r.clone(), PathBuf::from),
    })
}

/// True when `path` is `root` or lies inside it, as written or with links resolved on both sides,
/// case ignored only where file systems usually ignore it (macOS, Windows). A link inside `root`
/// (plugins are often symlinked in from a build folder) counts as inside, as it does for a full
/// scan, which follows links too. Unlike [`is_under`], which blocklists use and which may match
/// more than it should, this decides what may be loaded, so `/work/PLUGINS` is not inside
/// `/work/plugins` on Linux.
pub fn is_inside(path: &Path, root: &Path) -> bool {
    inside(path, root, false) || inside(path, root, true)
}

fn inside(path: &Path, root: &Path, resolve: bool) -> bool {
    let fold = cfg!(any(target_os = "macos", windows));
    let parts = |p: &Path| -> Vec<String> {
        let p = if resolve { resolved(p).unwrap_or_else(|| p.to_path_buf()) } else { p.to_path_buf() };
        p.components()
            .filter(|c| *c != Component::CurDir)
            .map(|c| {
                let s = c.as_os_str().to_string_lossy();
                if fold { s.to_lowercase() } else { s.into_owned() }
            })
            .collect()
    };
    let r = parts(root);
    !r.is_empty() && parts(path).starts_with(&r)
}

/// True when a blocklist entry covers the plugin at `path`. An absolute entry blocks that bundle
/// and everything inside a folder of that name, comparing links resolved on either side as well. A bare name blocks every bundle with that file name, with or without its extension
/// (`AutoAlign2.vst3` or `AutoAlign2`). Case is ignored; entries with an [`entry_problem`] never match.
pub fn is_blocked(path: &Path, entries: &[PathBuf]) -> bool {
    let name = path.file_name().map(|n| n.to_string_lossy().to_lowercase());
    let stem = path.file_stem().map(|n| n.to_string_lossy().to_lowercase());
    let mut real: Option<Option<PathBuf>> = None;
    entries.iter().filter(|e| entry_problem(e).is_none()).any(|e| {
        if e.is_absolute() {
            if is_under(path, e) {
                return true;
            }
            let Some(p) = real.get_or_insert_with(|| resolved(path)).as_deref() else { return false };
            is_under(p, e) || resolved(e).is_some_and(|e| is_under(p, &e))
        } else {
            let e = e.to_string_lossy().to_lowercase();
            name.as_deref() == Some(e.as_str()) || stem.as_deref() == Some(e.as_str())
        }
    })
}

/// The OS-specific standard locations, given the relevant environment values.
pub fn os_search_paths(home: Option<PathBuf>, common_program_files: Option<PathBuf>, local_app_data: Option<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    if cfg!(target_os = "macos") {
        if let Some(h) = home {
            v.push(h.join("Library/Audio/Plug-Ins/CLAP"));
        }
        v.push(PathBuf::from("/Library/Audio/Plug-Ins/CLAP"));
    } else if cfg!(windows) {
        if let Some(c) = common_program_files {
            v.push(c.join("CLAP"));
        }
        if let Some(l) = local_app_data {
            v.push(l.join("Programs").join("Common").join("CLAP"));
        }
    } else {
        if let Some(h) = home {
            v.push(h.join(".clap"));
        }
        v.push(PathBuf::from("/usr/lib/clap"));
        v.push(PathBuf::from("/usr/local/lib/clap"));
    }
    v
}

/// True when the path names a CLAP plugin (`*.clap`, case-insensitive).
pub fn is_clap_path(p: &Path) -> bool {
    p.extension().and_then(std::ffi::OsStr::to_str).is_some_and(|e| e.eq_ignore_ascii_case("clap"))
}

/// Recursively collects `.clap` files and bundles under `dirs` (missing directories are skipped).
/// A `.clap` directory is a bundle and is not descended into. The result is sorted and deduplicated.
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut budget = MAX_ENTRIES;
    for d in dirs {
        if is_clap_path(d) && d.exists() {
            out.push(d.clone());
        } else {
            walk(d, 0, &mut out, &mut budget);
        }
    }
    out.sort();
    out.dedup();
    out.truncate(MAX_BUNDLES);
    out
}

fn walk(dir: &Path, depth: usize, out: &mut Vec<PathBuf>, budget: &mut usize) {
    if depth > MAX_DEPTH || out.len() >= MAX_BUNDLES {
        return;
    }
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        if *budget == 0 || out.len() >= MAX_BUNDLES {
            return;
        }
        *budget -= 1;
        let p = entry.path();
        // `metadata` follows symlinks (plugins are often symlinked in); errors are skipped.
        let Ok(md) = std::fs::metadata(&p) else { continue };
        if is_clap_path(&p) {
            out.push(p);
        } else if md.is_dir() {
            walk(&p, depth + 1, out, budget);
        }
    }
}

/// The loadable binary of a plugin: the file itself, or for a macOS bundle directory
/// `Contents/MacOS/<name>` (the executable named like the bundle, else the only/first file there).
pub fn bundle_executable(path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if !path.is_dir() {
        return None;
    }
    let macos = path.join("Contents").join("MacOS");
    if let Some(stem) = path.file_stem() {
        let p = macos.join(stem);
        if p.is_file() {
            return Some(p);
        }
    }
    let mut files: Vec<PathBuf> = std::fs::read_dir(&macos).ok()?.flatten().map(|e| e.path()).filter(|p| p.is_file()).take(64).collect();
    files.sort();
    files.into_iter().next()
}

/// Maps CLAP feature strings to a SoundCraft insert-menu category.
pub fn category_from_features<S: AsRef<str>>(features: &[S]) -> Category {
    let has = |names: &[&str]| features.iter().any(|f| names.iter().any(|n| f.as_ref().eq_ignore_ascii_case(n)));
    if has(&["instrument", "synthesizer", "sampler", "drum", "drum-machine"]) {
        Category::Instrument
    } else if has(&["equalizer", "filter"]) {
        Category::Eq
    } else if has(&["compressor", "limiter", "expander", "gate", "de-esser", "transient-shaper"]) {
        Category::Dynamics
    } else if has(&["reverb"]) {
        Category::Reverb
    } else if has(&["delay"]) {
        Category::Delay
    } else if has(&["chorus", "flanger", "phaser", "tremolo"]) {
        Category::Modulation
    } else if has(&["distortion"]) {
        Category::Harmonic
    } else if has(&["pitch-shifter", "pitch-correction"]) {
        Category::PitchShift
    } else {
        Category::Other
    }
}

/// True when the features mark an instrument (CLAP `instrument`).
pub fn is_instrument<S: AsRef<str>>(features: &[S]) -> bool {
    features.iter().any(|f| f.as_ref().eq_ignore_ascii_case("instrument"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundcraft-clap-scan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_files_and_bundles_recursively_but_not_inside_bundles() {
        let d = tmp("find");
        std::fs::write(d.join("a.clap"), b"x").unwrap();
        std::fs::write(d.join("readme.txt"), b"x").unwrap();
        std::fs::create_dir_all(d.join("vendor/sub")).unwrap();
        std::fs::write(d.join("vendor/sub/B.CLAP"), b"x").unwrap();
        std::fs::create_dir_all(d.join("c.clap/Contents/MacOS")).unwrap();
        std::fs::write(d.join("c.clap/Contents/MacOS/c"), b"x").unwrap();
        std::fs::write(d.join("c.clap/Contents/inner.clap"), b"x").unwrap();
        let found = find_bundles(&[d.clone(), d.join("missing")]);
        let names: Vec<String> = found.iter().map(|p| p.strip_prefix(&d).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(names, vec!["a.clap", "c.clap", "vendor/sub/B.CLAP"]);
        assert_eq!(bundle_executable(&d.join("c.clap")), Some(d.join("c.clap/Contents/MacOS/c")));
        assert_eq!(bundle_executable(&d.join("a.clap")), Some(d.join("a.clap")));
        assert_eq!(bundle_executable(&d.join("nope.clap")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bundle_executable_falls_back_to_first_file() {
        let d = tmp("exe");
        std::fs::create_dir_all(d.join("X.clap/Contents/MacOS")).unwrap();
        std::fs::write(d.join("X.clap/Contents/MacOS/other"), b"x").unwrap();
        assert_eq!(bundle_executable(&d.join("X.clap")), Some(d.join("X.clap/Contents/MacOS/other")));
        std::fs::create_dir_all(d.join("Empty.clap")).unwrap();
        assert_eq!(bundle_executable(&d.join("Empty.clap")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn os_paths_cover_the_standard_locations() {
        let v = os_search_paths(Some("/home/u".into()), Some("C:/CPF".into()), Some("C:/LAD".into()));
        if cfg!(target_os = "macos") {
            assert_eq!(v, vec![PathBuf::from("/home/u/Library/Audio/Plug-Ins/CLAP"), PathBuf::from("/Library/Audio/Plug-Ins/CLAP")]);
        } else if cfg!(windows) {
            assert_eq!(v.len(), 2);
            assert!(v[0].ends_with("CLAP") && v[1].ends_with("CLAP"));
        } else {
            assert_eq!(v, vec![PathBuf::from("/home/u/.clap"), PathBuf::from("/usr/lib/clap"), PathBuf::from("/usr/local/lib/clap")]);
        }
        if !env_flag(PATHS_ONLY_ENV) {
            assert!(default_search_paths().len() >= v.len().saturating_sub(1));
        }
    }

    /// A test path made absolute on every platform (Windows paths need a drive).
    fn abs(p: &str) -> PathBuf {
        if cfg!(windows) && p.starts_with('/') { PathBuf::from(format!("C:{p}")) } else { PathBuf::from(p) }
    }

    #[test]
    fn env_paths_extend_or_replace_the_standard_locations() {
        let std = vec![PathBuf::from("/Library/Audio/Plug-Ins/CLAP")];
        let env = std::env::join_paths(["/a", "", "/b"]).unwrap();
        assert_eq!(search_paths(std.clone(), Some(&env), false), vec![PathBuf::from("/Library/Audio/Plug-Ins/CLAP"), "/a".into(), "/b".into()]);
        assert_eq!(search_paths(std.clone(), Some(&env), true), vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert!(search_paths(std, None, true).is_empty(), "paths-only without a path list scans nothing");
    }

    #[test]
    fn blocklist_entries_match_names_and_paths() {
        let p = abs("/Library/Audio/Plug-Ins/CLAP/vendor/Foo.clap");
        let v = |e: &[&str]| e.iter().map(|e| abs(e)).collect::<Vec<_>>();
        assert!(is_blocked(&p, &v(&["Foo.clap"])));
        assert!(is_blocked(&p, &v(&["FOO"])));
        assert!(!is_blocked(&p, &v(&["Fo"])));
        assert!(is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/CLAP/vendor"])));
        assert!(!is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/CLAP/vend"])));
        assert!(is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/./CLAP/VENDOR/"])), "`.` segments and case don't matter");
        assert!(!is_blocked(&p, &v(&["/", ".", "vendor/Foo.clap", "/Library/x/../Audio"])), "unusable entries never match");
        assert!(!is_blocked(&p, &[]));
    }

    #[cfg(unix)]
    #[test]
    fn loading_admission_is_exact_and_follows_links() {
        let d = tmp("inside");
        std::fs::create_dir_all(d.join("plugins/vendor")).unwrap();
        std::fs::create_dir_all(d.join("PLUGINS")).unwrap();
        std::os::unix::fs::symlink(d.join("plugins"), d.join("alias")).unwrap();
        assert!(is_inside(&d.join("plugins/vendor"), &d.join("plugins")));
        assert!(is_inside(&d.join("alias/vendor"), &d.join("plugins")), "through a link");
        // A bundle linked into the folder from a build tree elsewhere is inside, as for a full scan.
        std::fs::create_dir_all(d.join("build/Dev.clap")).unwrap();
        std::os::unix::fs::symlink(d.join("build/Dev.clap"), d.join("plugins/vendor/Dev.clap")).unwrap();
        assert!(is_inside(&d.join("plugins/vendor/Dev.clap"), &d.join("plugins")), "a linked-in bundle");
        assert!(!is_inside(&d.join("build/Dev.clap"), &d.join("plugins")), "but not its target by itself");
        assert!(!is_inside(&d.join("plugin"), &d.join("plugins")));
        if !cfg!(target_os = "macos") {
            assert!(!is_inside(&d.join("PLUGINS"), &d.join("plugins")), "case matters where the file system says so");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn entries_are_checked_before_use() {
        assert_eq!(entry_problem(Path::new("Foo.clap")), None);
        assert_eq!(entry_problem(&abs("/Library/Audio/Plug-Ins/CLAP")), None);
        assert!(entry_problem(Path::new("/")).is_some());
        assert!(entry_problem(Path::new("vendor/Foo.clap")).is_some());
        assert!(entry_problem(Path::new("/a/../b")).is_some());
        assert!(is_under(Path::new("/a/b/c.clap"), Path::new("/a/b")));
        assert!(!is_under(Path::new("/a/bc.clap"), Path::new("/a/b")));
    }

    #[test]
    fn features_map_to_categories() {
        assert_eq!(category_from_features(&["audio-effect", "reverb", "stereo"]), Category::Reverb);
        assert_eq!(category_from_features(&["instrument", "synthesizer"]), Category::Instrument);
        assert_eq!(category_from_features(&["audio-effect", "Compressor"]), Category::Dynamics);
        assert_eq!(category_from_features(&["audio-effect", "equalizer"]), Category::Eq);
        assert_eq!(category_from_features(&["chorus"]), Category::Modulation);
        assert_eq!(category_from_features(&["distortion"]), Category::Harmonic);
        assert_eq!(category_from_features(&["pitch-shifter"]), Category::PitchShift);
        assert_eq!(category_from_features(&["delay"]), Category::Delay);
        assert_eq!(category_from_features::<&str>(&[]), Category::Other);
        assert!(is_instrument(&["instrument"]));
        assert!(!is_instrument(&["audio-effect"]));
    }
}
