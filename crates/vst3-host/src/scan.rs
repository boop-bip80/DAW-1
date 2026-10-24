//! Finding VST3 plugins on disk (safe, pure `std::fs`), resolving bundles to their binaries and
//! mapping VST3 sub-categories to SoundCraft categories.

use soundcraft_dsp::Category;
use std::path::{Component, Path, PathBuf};

/// Directory recursion limit while scanning (vendors nest folders, never deeply).
const MAX_DEPTH: usize = 8;
/// Upper bound on bundles collected by one scan (hostile or huge folders).
const MAX_BUNDLES: usize = 4096;
/// Upper bound on directory entries visited by one scan.
const MAX_ENTRIES: usize = 100_000;

/// The operating systems whose VST3 bundle layouts we know.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    MacOs,
    Windows,
    /// Linux and the BSDs (same bundle layout).
    Unix,
}

impl Os {
    /// The OS this build runs on.
    pub fn current() -> Os {
        if cfg!(target_os = "macos") {
            Os::MacOs
        } else if cfg!(windows) {
            Os::Windows
        } else {
            Os::Unix
        }
    }
}

/// Set (to anything but `0` or empty) to scan only the `VST3_PATH` / `CLAP_PATH` folders and skip
/// the standard locations: a test harness then loads only the plugins it names.
pub const PATHS_ONLY_ENV: &str = "SOUNDCRAFT_PLUGIN_PATHS_ONLY";
/// A path list (like `PATH`) of plugins and folders never loaded; see [`is_blocked`].
pub const BLOCKLIST_ENV: &str = "SOUNDCRAFT_PLUGIN_BLOCKLIST";

/// The standard VST3 search locations for this OS (existing or not), followed by every entry of
/// the `VST3_PATH` environment variable. With [`PATHS_ONLY_ENV`] set, only the `VST3_PATH` entries.
pub fn default_search_paths() -> Vec<PathBuf> {
    let os = os_search_paths(
        Os::current(),
        std::env::var_os("HOME").map(PathBuf::from),
        std::env::var_os("COMMONPROGRAMFILES").map(PathBuf::from),
        std::env::var_os("LOCALAPPDATA").map(PathBuf::from),
    );
    search_paths(os, std::env::var_os("VST3_PATH").as_deref(), env_flag(PATHS_ONLY_ENV))
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

/// True when `path` is `root` or lies inside it as the file system sees them: links resolved on
/// both sides, case ignored only where file systems usually ignore it (macOS, Windows). Unlike
/// [`is_under`], which blocklists use and which may match more than it should, this decides what
/// may be loaded, so `/work/PLUGINS` is not inside `/work/plugins` on Linux.
pub fn is_inside(path: &Path, root: &Path) -> bool {
    let fold = cfg!(any(target_os = "macos", windows));
    let parts = |p: &Path| -> Vec<String> {
        let p = resolved(p).unwrap_or_else(|| p.to_path_buf());
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
pub fn os_search_paths(os: Os, home: Option<PathBuf>, common_program_files: Option<PathBuf>, local_app_data: Option<PathBuf>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    match os {
        Os::MacOs => {
            if let Some(h) = home {
                v.push(h.join("Library/Audio/Plug-Ins/VST3"));
            }
            v.push(PathBuf::from("/Library/Audio/Plug-Ins/VST3"));
        }
        Os::Windows => {
            if let Some(c) = common_program_files {
                v.push(c.join("VST3"));
            }
            if let Some(l) = local_app_data {
                v.push(l.join("Programs").join("Common").join("VST3"));
            }
        }
        Os::Unix => {
            if let Some(h) = home {
                v.push(h.join(".vst3"));
            }
            v.push(PathBuf::from("/usr/lib/vst3"));
            v.push(PathBuf::from("/usr/local/lib/vst3"));
        }
    }
    v
}

/// True when the path names a VST3 plugin (`*.vst3`, case-insensitive).
pub fn is_vst3_path(p: &Path) -> bool {
    p.extension().and_then(std::ffi::OsStr::to_str).is_some_and(|e| e.eq_ignore_ascii_case("vst3"))
}

/// Recursively collects `.vst3` bundles and files under `dirs` (missing directories are skipped).
/// A `.vst3` directory is a bundle and is not descended into. The result is sorted and deduplicated.
pub fn find_bundles(dirs: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut budget = MAX_ENTRIES;
    for d in dirs {
        if is_vst3_path(d) && d.exists() {
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
        if is_vst3_path(&p) {
            out.push(p);
        } else if md.is_dir() {
            walk(&p, depth + 1, out, budget);
        }
    }
}

/// The `Contents/<dir>` folder names that hold this machine's binaries, most specific first.
fn arch_dirs(os: Os) -> &'static [&'static str] {
    match os {
        Os::MacOs => &["MacOS"],
        Os::Windows => {
            if cfg!(target_arch = "aarch64") {
                &["arm64-win", "arm64ec-win", "arm64x-win"]
            } else if cfg!(target_arch = "x86") {
                &["x86-win"]
            } else {
                &["x86_64-win"]
            }
        }
        Os::Unix => {
            if cfg!(target_arch = "aarch64") {
                &["aarch64-linux"]
            } else if cfg!(target_arch = "x86") {
                &["i386-linux"]
            } else {
                &["x86_64-linux"]
            }
        }
    }
}

/// The loadable binary of a plugin for this OS: a single-file `.vst3` itself, or inside a bundle
/// directory `Contents/MacOS/<name>` (macOS), `Contents/x86_64-win/<name>.vst3` (Windows) or
/// `Contents/x86_64-linux/<name>.so` (Linux/BSD), falling back to the only/first fitting file there.
pub fn bundle_binary(path: &Path) -> Option<PathBuf> {
    bundle_binary_for(Os::current(), path)
}

/// [`bundle_binary`] for a given OS layout (testable everywhere).
pub fn bundle_binary_for(os: Os, path: &Path) -> Option<PathBuf> {
    if path.is_file() {
        return Some(path.to_path_buf());
    }
    if !path.is_dir() {
        return None;
    }
    let stem = path.file_stem()?;
    let ext = match os {
        Os::MacOs => None,
        Os::Windows => Some("vst3"),
        Os::Unix => Some("so"),
    };
    for dir in arch_dirs(os) {
        let folder = path.join("Contents").join(dir);
        let mut named = folder.join(stem);
        if let Some(e) = ext {
            named.set_extension(e);
        }
        if named.is_file() {
            return Some(named);
        }
        let Ok(rd) = std::fs::read_dir(&folder) else { continue };
        let mut files: Vec<PathBuf> = rd
            .flatten()
            .map(|e| e.path())
            .filter(|p| p.is_file())
            .filter(|p| ext.is_none_or(|e| p.extension().and_then(std::ffi::OsStr::to_str).is_some_and(|x| x.eq_ignore_ascii_case(e))))
            .take(64)
            .collect();
        files.sort();
        if let Some(f) = files.into_iter().next() {
            return Some(f);
        }
    }
    None
}

/// The `|`-separated VST3 sub-category tokens (`"Fx|Reverb"` → `["Fx", "Reverb"]`).
pub fn split_sub_categories(s: &str) -> Vec<String> {
    s.split('|').map(str::trim).filter(|t| !t.is_empty()).take(32).map(str::to_string).collect()
}

/// Maps VST3 sub-categories (`Fx|EQ`, `Instrument|Synth`, …) to a SoundCraft insert-menu category.
pub fn category_from_sub_categories<S: AsRef<str>>(subs: &[S]) -> Category {
    let has = |names: &[&str]| subs.iter().any(|f| names.iter().any(|n| f.as_ref().eq_ignore_ascii_case(n)));
    if is_instrument(subs) {
        Category::Instrument
    } else if has(&["EQ", "Filter"]) {
        Category::Eq
    } else if has(&["Dynamics"]) {
        Category::Dynamics
    } else if has(&["Reverb"]) {
        Category::Reverb
    } else if has(&["Delay"]) {
        Category::Delay
    } else if has(&["Modulation"]) {
        Category::Modulation
    } else if has(&["Distortion"]) {
        Category::Harmonic
    } else if has(&["Pitch Shift"]) {
        Category::PitchShift
    } else {
        Category::Other
    }
}

/// True when the sub-categories mark an instrument (`Instrument`, `Instrument|Synth`, …).
pub fn is_instrument<S: AsRef<str>>(subs: &[S]) -> bool {
    subs.first().is_some_and(|f| f.as_ref().eq_ignore_ascii_case("Instrument"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("soundcraft-vst3-scan-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn finds_files_and_bundles_recursively_but_not_inside_bundles() {
        let d = tmp("find");
        std::fs::write(d.join("a.vst3"), b"x").unwrap();
        std::fs::write(d.join("readme.txt"), b"x").unwrap();
        std::fs::create_dir_all(d.join("vendor/sub")).unwrap();
        std::fs::write(d.join("vendor/sub/B.VST3"), b"x").unwrap();
        std::fs::create_dir_all(d.join("c.vst3/Contents/MacOS")).unwrap();
        std::fs::write(d.join("c.vst3/Contents/MacOS/c"), b"x").unwrap();
        std::fs::write(d.join("c.vst3/Contents/inner.vst3"), b"x").unwrap();
        let found = find_bundles(&[d.clone(), d.join("missing")]);
        let names: Vec<String> = found.iter().map(|p| p.strip_prefix(&d).unwrap().to_string_lossy().replace('\\', "/")).collect();
        assert_eq!(names, vec!["a.vst3", "c.vst3", "vendor/sub/B.VST3"]);
        assert_eq!(find_bundles(&[d.join("a.vst3")]), vec![d.join("a.vst3")]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn bundles_resolve_per_os_layout() {
        let d = tmp("layout");
        // macOS
        std::fs::create_dir_all(d.join("M.vst3/Contents/MacOS")).unwrap();
        std::fs::write(d.join("M.vst3/Contents/MacOS/M"), b"x").unwrap();
        std::fs::write(d.join("M.vst3/Contents/MacOS/aaa"), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::MacOs, &d.join("M.vst3")), Some(d.join("M.vst3/Contents/MacOS/M")));
        // macOS fallback: first file when the name differs.
        std::fs::create_dir_all(d.join("N.vst3/Contents/MacOS")).unwrap();
        std::fs::write(d.join("N.vst3/Contents/MacOS/other"), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::MacOs, &d.join("N.vst3")), Some(d.join("N.vst3/Contents/MacOS/other")));
        // Windows
        let win = arch_dirs(Os::Windows)[0];
        std::fs::create_dir_all(d.join(format!("W.vst3/Contents/{win}"))).unwrap();
        std::fs::write(d.join(format!("W.vst3/Contents/{win}/W.vst3")), b"x").unwrap();
        std::fs::write(d.join(format!("W.vst3/Contents/{win}/helper.dll")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Windows, &d.join("W.vst3")), Some(d.join(format!("W.vst3/Contents/{win}/W.vst3"))));
        // Linux
        let lin = arch_dirs(Os::Unix)[0];
        std::fs::create_dir_all(d.join(format!("L.vst3/Contents/{lin}"))).unwrap();
        std::fs::write(d.join(format!("L.vst3/Contents/{lin}/notes.txt")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Unix, &d.join("L.vst3")), None, "only .so files qualify");
        std::fs::write(d.join(format!("L.vst3/Contents/{lin}/Renamed.so")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Unix, &d.join("L.vst3")), Some(d.join(format!("L.vst3/Contents/{lin}/Renamed.so"))));
        std::fs::write(d.join(format!("L.vst3/Contents/{lin}/L.so")), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Unix, &d.join("L.vst3")), Some(d.join(format!("L.vst3/Contents/{lin}/L.so"))));
        // Single-file plugin, wrong layout, missing.
        std::fs::write(d.join("single.vst3"), b"x").unwrap();
        assert_eq!(bundle_binary_for(Os::Windows, &d.join("single.vst3")), Some(d.join("single.vst3")));
        assert_eq!(bundle_binary_for(Os::Windows, &d.join("M.vst3")), None);
        assert_eq!(bundle_binary_for(Os::MacOs, &d.join("nope.vst3")), None);
        std::fs::create_dir_all(d.join("Empty.vst3")).unwrap();
        assert_eq!(bundle_binary(&d.join("Empty.vst3")), None);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn os_paths_cover_the_standard_locations() {
        let mac = os_search_paths(Os::MacOs, Some("/Users/u".into()), None, None);
        assert_eq!(mac, vec![PathBuf::from("/Users/u/Library/Audio/Plug-Ins/VST3"), PathBuf::from("/Library/Audio/Plug-Ins/VST3")]);
        let win = os_search_paths(Os::Windows, None, Some("C:/CPF".into()), Some("C:/LAD".into()));
        assert_eq!(win[0], PathBuf::from("C:/CPF").join("VST3"));
        assert_eq!(win.len(), 2);
        let unix = os_search_paths(Os::Unix, Some("/home/u".into()), None, None);
        assert_eq!(unix, vec![PathBuf::from("/home/u/.vst3"), PathBuf::from("/usr/lib/vst3"), PathBuf::from("/usr/local/lib/vst3")]);
        if !env_flag(PATHS_ONLY_ENV) {
            assert!(!default_search_paths().is_empty());
        }
    }

    /// A test path made absolute on every platform (Windows paths need a drive).
    fn abs(p: &str) -> PathBuf {
        if cfg!(windows) && p.starts_with('/') { PathBuf::from(format!("C:{p}")) } else { PathBuf::from(p) }
    }

    #[test]
    fn env_paths_extend_or_replace_the_standard_locations() {
        let std = vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3")];
        let env = std::env::join_paths(["/a", "", "/b"]).unwrap();
        assert_eq!(search_paths(std.clone(), Some(&env), false), vec![PathBuf::from("/Library/Audio/Plug-Ins/VST3"), "/a".into(), "/b".into()]);
        assert_eq!(search_paths(std.clone(), Some(&env), true), vec![PathBuf::from("/a"), PathBuf::from("/b")]);
        assert_eq!(search_paths(std.clone(), None, false), std);
        assert!(search_paths(std, None, true).is_empty(), "paths-only without a path list scans nothing");
    }

    #[test]
    fn blocklist_entries_match_names_and_paths() {
        let p = abs("/Library/Audio/Plug-Ins/VST3/AutoAlign2.vst3");
        let v = |e: &[&str]| e.iter().map(|e| abs(e)).collect::<Vec<_>>();
        assert!(is_blocked(&p, &v(&["AutoAlign2.vst3"])));
        assert!(is_blocked(&p, &v(&["autoalign2"])), "name without extension, any case");
        assert!(!is_blocked(&p, &v(&["AutoAlign"])), "names match whole, not as prefixes");
        assert!(is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/VST3/AutoAlign2.vst3"])));
        assert!(is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/VST3"])), "a folder blocks what it holds");
        assert!(is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/VST3/"])));
        assert!(!is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/VST"])), "path prefixes stop at separators");
        assert!(!is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/VST3/forwardaudio"])));
        assert!(is_blocked(&p, &v(&["/Library/Audio/Plug-Ins/./VST3/autoalign2.VST3"])), "`.` segments and case don't matter");
        assert!(is_blocked(&p, &v(&["//Library//Audio/Plug-Ins/VST3"])));
        assert!(!is_blocked(&p, &v(&["/", ".", "VST3/AutoAlign2.vst3", "./AutoAlign2.vst3", "/Library/x/../Audio"])), "unusable entries never match");
        assert!(!is_blocked(&p, &[]));
    }

    #[cfg(windows)]
    #[test]
    fn blocklist_paths_match_either_separator_on_windows() {
        let p = PathBuf::from(r"C:\Program Files\Common Files\VST3\Foo.vst3");
        assert!(is_blocked(&p, &[PathBuf::from("C:/Program Files/Common Files/VST3")]));
        assert!(is_blocked(&p, &[PathBuf::from(r"c:\program files\common files\vst3\foo.vst3")]));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_blocked_bundle_is_blocked_too() {
        let d = tmp("link");
        std::fs::create_dir_all(d.join("real/X.vst3")).unwrap();
        std::fs::create_dir_all(d.join("links")).unwrap();
        std::os::unix::fs::symlink(d.join("real/X.vst3"), d.join("links/Y.vst3")).unwrap();
        let real = std::fs::canonicalize(d.join("real")).unwrap();
        assert!(is_blocked(&d.join("links/Y.vst3"), &[real]));
        assert!(!is_blocked(&d.join("links/Y.vst3"), &[d.join("other")]));
        // And the other way round: an entry naming a link covers the folder it leads to.
        std::os::unix::fs::symlink(d.join("real"), d.join("alias")).unwrap();
        assert!(is_blocked(&std::fs::canonicalize(d.join("real/X.vst3")).unwrap(), &[d.join("alias")]));
        let _ = std::fs::remove_dir_all(&d);
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
        assert!(!is_inside(&d.join("plugin"), &d.join("plugins")));
        if !cfg!(target_os = "macos") {
            assert!(!is_inside(&d.join("PLUGINS"), &d.join("plugins")), "case matters where the file system says so");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn entries_are_checked_before_use() {
        assert_eq!(entry_problem(Path::new("AutoAlign2.vst3")), None);
        assert_eq!(entry_problem(&abs("/Library/Audio/Plug-Ins/VST3")), None);
        assert!(entry_problem(Path::new("/")).is_some());
        assert!(entry_problem(Path::new("VST3/Foo.vst3")).is_some());
        assert!(entry_problem(Path::new("./Foo.vst3")).is_some());
        assert!(entry_problem(Path::new("/a/../b")).is_some());
        assert!(is_under(Path::new("/a/b/c.vst3"), Path::new("/a/b")));
        assert!(!is_under(Path::new("/a/bc.vst3"), Path::new("/a/b")));
        assert!(!is_under(Path::new("/a"), Path::new("")));
    }

    #[test]
    fn sub_categories_map_to_categories() {
        let c = |s: &str| category_from_sub_categories(&split_sub_categories(s));
        assert_eq!(c("Fx|Reverb"), Category::Reverb);
        assert_eq!(c("Instrument|Synth"), Category::Instrument);
        assert_eq!(c("Fx|Dynamics"), Category::Dynamics);
        assert_eq!(c("Fx|EQ"), Category::Eq);
        assert_eq!(c("Fx|Filter"), Category::Eq);
        assert_eq!(c("Fx|Modulation"), Category::Modulation);
        assert_eq!(c("Fx|Distortion"), Category::Harmonic);
        assert_eq!(c("Fx|Pitch Shift"), Category::PitchShift);
        assert_eq!(c("Fx|Delay"), Category::Delay);
        assert_eq!(c("Fx"), Category::Other);
        assert_eq!(c(""), Category::Other);
        assert_eq!(split_sub_categories(" Fx || Delay |"), vec!["Fx", "Delay"]);
        assert!(is_instrument(&split_sub_categories("Instrument|Sampler")));
        assert!(!is_instrument(&split_sub_categories("Fx|Instrument")));
    }
}
