//! Which third-party plugins get loaded: the blocklist shared by the CLAP and VST3 hosts
//! (`engine.plugin_blocklist`) and the path lists that `engine.clap_plugins` /
//! `engine.vst3_plugins` take to rescan only some plugins. Audio Units are listed by the system,
//! not by loading binaries, so they are not affected.
use super::{CommandSpec, always, bad};
use crate::{Result, cmd};

use serde_json::{Value, json};
use std::path::{Component, Path, PathBuf};

/// Most entries one path list may hold (hostile input).
const MAX_ENTRIES: usize = 1024;
/// Longest accepted entry, in bytes.
const MAX_ENTRY_LEN: usize = 4096;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        query "engine.plugin_blocklist",
        "Plugin Blocklist",
        [],
        None,
        "{set?: [entry], add?: [entry], remove?: [entry]} → {blocklist: [entry]}, applied in that order. Blocked CLAP and VST3 \
         plugins are never loaded. An entry is an absolute bundle or folder path (blocks it and what it holds, also through links) \
         or a bare bundle name, with or without extension (`AutoAlign2.vst3`, `AutoAlign2`); case and `.` segments are ignored, \
         and so are duplicates. Starts as `SOUNDCRAFT_PLUGIN_BLOCKLIST` (a path list). Newly blocked plugins disappear from the \
         lists at once; unblocked ones return with the next rescan of their folder.",
        always,
        |_, p| {
            const ID: &str = "engine.plugin_blocklist";
            let mut list = match p.get("set") {
                Some(v) => entry_list(ID, "set", v)?,
                None => soundcraft_vst3_host::blocklist(),
            };
            if let Some(v) = p.get("add") {
                list.extend(entry_list(ID, "add", v)?);
            }
            if let Some(v) = p.get("remove") {
                let gone: Vec<String> = entry_list(ID, "remove", v)?.iter().map(|e| key(e)).collect();
                list.retain(|e| !gone.contains(&key(e)));
            }
            let mut seen = Vec::new();
            list.retain(|e| {
                let k = key(e);
                let new = !seen.contains(&k);
                seen.push(k);
                new
            });
            if list.len() > MAX_ENTRIES {
                return Err(bad(ID, format!("the blocklist holds at most {MAX_ENTRIES} entries")));
            }
            soundcraft_clap_host::set_blocklist(list.clone());
            soundcraft_vst3_host::set_blocklist(list.clone());
            Ok(json!({"blocklist": list.iter().map(|e| e.to_string_lossy()).collect::<Vec<_>>()}))
        }
    )]
}

/// An entry compared the way the hosts match it: `.` dropped, case ignored.
fn key(e: &Path) -> String {
    e.components().filter(|c| *c != Component::CurDir).map(|c| c.as_os_str().to_string_lossy().to_lowercase()).collect::<Vec<_>>().join("/")
}

/// A JSON array of non-empty path strings, bounded in count and length.
pub fn path_list(cmd: &str, key: &str, v: &Value) -> Result<Vec<PathBuf>> {
    let items = v.as_array().ok_or_else(|| bad(cmd, format!("`{key}` must be an array of paths")))?;
    if items.len() > MAX_ENTRIES {
        return Err(bad(cmd, format!("`{key}` holds at most {MAX_ENTRIES} entries")));
    }
    items
        .iter()
        .map(|i| match i.as_str() {
            Some(s) if !s.is_empty() && s.len() <= MAX_ENTRY_LEN => Ok(PathBuf::from(s)),
            Some(_) => Err(bad(cmd, format!("`{key}` entries must be 1 to {MAX_ENTRY_LEN} bytes long"))),
            None => Err(bad(cmd, format!("`{key}` must be an array of paths"))),
        })
        .collect()
}

/// A [`path_list`] of blocklist entries: absolute paths or bare bundle names.
fn entry_list(cmd: &str, key: &str, v: &Value) -> Result<Vec<PathBuf>> {
    let list = path_list(cmd, key, v)?;
    if let Some((e, why)) = list.iter().find_map(|e| soundcraft_vst3_host::scan::entry_problem(e).map(|w| (e, w))) {
        return Err(bad(cmd, format!("`{key}` entry `{}` {why}", e.display())));
    }
    Ok(list)
}

/// What `rescan` asks for: nothing, everything (`true`) or only some bundles and folders (a path list).
pub enum Rescan {
    No,
    All,
    Paths(Vec<PathBuf>),
}

/// Reads `rescan`. Its paths load plugin binaries, so they must be absolute, inside the format's
/// search path (`in_search_path`: a rescan never loads what a full scan wouldn't) and readable
/// under the automation roots in force (`--automation-read-root`), like any file a command reads.
pub fn rescan_param(cmd: &str, p: &Value, in_search_path: fn(&Path) -> bool) -> Result<Rescan> {
    match p.get("rescan") {
        None | Some(Value::Null) | Some(Value::Bool(false)) => Ok(Rescan::No),
        Some(Value::Bool(true)) => Ok(Rescan::All),
        Some(v) => {
            let list = path_list(cmd, "rescan", v)?;
            for t in &list {
                if !t.is_absolute() || soundcraft_vst3_host::scan::entry_problem(t).is_some() {
                    return Err(bad(cmd, format!("`rescan` entry `{}` must be an absolute bundle or folder path", t.display())));
                }
                if !in_search_path(t) {
                    return Err(bad(
                        cmd,
                        format!("`rescan` entry `{}` is not in the plugin search path (the standard folders and the *_PATH folders)", t.display()),
                    ));
                }
                crate::file_access::check_read(t).map_err(crate::EngineError::Io)?;
            }
            Ok(Rescan::Paths(list))
        }
    }
}

#[cfg(test)]
mod tests {
    use crate::Engine;
    use serde_json::json;

    #[test]
    fn blocklist_edits_apply_to_both_hosts() {
        let mut e = Engine::default();
        let before = e.execute("engine.plugin_blocklist", &json!({})).unwrap()["blocklist"].clone();
        // Absolute on every platform (Windows paths need a drive).
        let abs = |p: &str| if cfg!(windows) { format!("C:{p}") } else { p.to_string() };
        let (a, a2, c) = (abs("/nowhere/A.vst3"), abs("/nowhere/./a.VST3"), abs("/nowhere/C.clap"));
        let r = e.execute("engine.plugin_blocklist", &json!({"set": [a, a2], "add": ["B", "b", c]})).unwrap();
        assert_eq!(r["blocklist"], json!([a, "B", c]), "duplicates by case and `.` dropped");
        let r = e.execute("engine.plugin_blocklist", &json!({"remove": ["b"]})).unwrap();
        assert_eq!(r["blocklist"], json!([a, c]), "removal ignores case too");
        assert_eq!(soundcraft_clap_host::blocklist(), soundcraft_vst3_host::blocklist());
        e.execute("engine.plugin_blocklist", &json!({"set": before})).unwrap();
    }

    #[test]
    fn bad_path_lists_are_errors() {
        let mut e = Engine::default();
        for p in [
            json!({"add": "A.vst3"}),
            json!({"set": [1]}),
            json!({"remove": [""]}),
            json!({"add": ["x".repeat(5000)]}),
            json!({"add": ["/"]}),
            json!({"add": ["vendor/A.vst3"]}),
            json!({"add": ["/a/../b"]}),
        ] {
            assert!(e.execute("engine.plugin_blocklist", &p).is_err(), "{p}");
        }
        assert!(e.execute("engine.vst3_plugins", &json!({"rescan": "yes"})).is_err());
        assert!(e.execute("engine.clap_plugins", &json!({"rescan": [3]})).is_err());
        assert!(e.execute("engine.vst3_plugins", &json!({"rescan": ["Fixture.vst3"]})).is_err(), "relative targets would match anywhere");
        assert!(e.execute("engine.clap_plugins", &json!({"rescan": ["/"]})).is_err());
    }

    #[test]
    fn rescan_paths_stay_inside_the_automation_read_roots() {
        let base = std::env::temp_dir().join(format!("soundcraft-rescan-roots-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(base.join("in")).unwrap();
        std::fs::create_dir_all(base.join("elsewhere")).unwrap();
        // Both folders are in the search path, so only the read roots tell them apart.
        for d in ["in", "elsewhere"] {
            soundcraft_vst3_host::register_search_dir(&base.join(d));
            soundcraft_clap_host::register_search_dir(&base.join(d));
        }
        let roots = crate::file_access::AutomationRoots::new(&[&base.join("in")], &[]).unwrap();
        crate::file_access::confine(roots.as_ref(), || {
            let mut e = Engine::default();
            let inside = json!({"rescan": [base.join("in").to_string_lossy()]});
            let outside = json!({"rescan": [base.join("elsewhere").to_string_lossy()]});
            assert_eq!(e.execute("engine.vst3_plugins", &inside).unwrap(), json!([]));
            assert!(e.execute("engine.vst3_plugins", &outside).is_err(), "no plugin binaries from outside the read roots");
            assert!(e.execute("engine.clap_plugins", &outside).is_err());
        });
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn rescan_targets_must_be_in_the_search_path() {
        let mut e = Engine::default();
        let base = std::env::temp_dir().join(format!("soundcraft-rescan-search-{}", std::process::id()));
        let p = json!({"rescan": [base.join("vendor").to_string_lossy()]});
        let err = e.execute("engine.vst3_plugins", &p).unwrap_err().to_string();
        assert!(err.contains("search path"), "{err}");
        assert!(e.execute("engine.clap_plugins", &p).is_err());
        soundcraft_vst3_host::register_search_dir(&base);
        soundcraft_clap_host::register_search_dir(&base);
        assert_eq!(e.execute("engine.vst3_plugins", &p).unwrap(), json!([]), "a missing folder in the search path holds nothing");
        assert_eq!(e.execute("engine.clap_plugins", &p).unwrap(), json!([]));
    }
}
