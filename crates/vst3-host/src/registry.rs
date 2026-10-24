//! The process-wide VST3 registry: scan cache, leaked `PluginInfo`s, instantiation.

use crate::ffi::{Bundle, Instance, PARAM_BYPASS, PARAM_HIDDEN, PARAM_READ_ONLY, RawParam};
use crate::plugin::Vst3Plugin;
use crate::scan as paths;
use crate::{AUDIO_MODULE_CLASS, Vst3Descriptor, Vst3Error, parse_id};
use soundcraft_dsp::{ParamInfo, Plugin, PluginInfo, Taper, Unit};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock, PoisonError};

/// Most labels read for a stepped parameter (more steps are shown as a plain range).
const MAX_CHOICES: i32 = 128;

#[derive(Default)]
struct Registry {
    scanned: Option<Vec<Vst3Descriptor>>,
    extra_dirs: Vec<PathBuf>,
    /// One leaked `PluginInfo` per plugin type that was actually instantiated (bounded by the
    /// number of distinct installed plugins).
    infos: HashMap<String, &'static PluginInfo>,
    /// Ids that failed to instantiate (not retried every UI frame).
    failed: HashSet<String>,
    /// Plugins and folders never loaded ([`paths::is_blocked`]); read from the environment on
    /// first use, then replaced by [`set_blocklist`].
    blocked: Option<Vec<PathBuf>>,
    /// Plugins found by [`rescan_paths`]: creatable before any full scan without loading anything
    /// else, and preferred over another bundle with the same id (a rebuilt plugin over the
    /// installed one) until a full [`rescan`].
    targeted: Vec<Vst3Descriptor>,
}

impl Registry {
    fn blocked(&mut self) -> &[PathBuf] {
        self.blocked.get_or_insert_with(paths::blocklist_from_env)
    }
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry::default()))
}

/// Descriptors of every audio module class in one `.vst3`.
pub fn load_bundle(path: &Path) -> Result<Vec<Vst3Descriptor>, Vst3Error> {
    let b = Bundle::load(path)?;
    Ok(b.classes()
        .into_iter()
        .filter(|c| c.category == AUDIO_MODULE_CLASS)
        .map(|c| {
            let subs = paths::split_sub_categories(&c.sub_categories);
            let class_id = crate::ffi::hex(&c.cid);
            Vst3Descriptor {
                id: format!("{}{class_id}", crate::ID_PREFIX),
                category: paths::category_from_sub_categories(&subs),
                is_instrument: paths::is_instrument(&subs),
                class_id,
                name: if c.name.is_empty() { "VST3 plugin".to_string() } else { c.name },
                vendor: c.vendor,
                version: c.version,
                sdk_version: c.sdk_version,
                sub_categories: subs,
                path: path.to_path_buf(),
            }
        })
        .collect())
}

/// Scans `dirs` (uncached). Blocked bundles are never loaded; unloadable files are logged and
/// skipped; the first plugin with a given id wins.
pub fn scan_paths(dirs: &[PathBuf]) -> Vec<Vst3Descriptor> {
    let blocked = registry().lock().unwrap_or_else(PoisonError::into_inner).blocked().to_vec();
    scan_dirs(dirs, &blocked)
}

fn scan_dirs(dirs: &[PathBuf], blocked: &[PathBuf]) -> Vec<Vst3Descriptor> {
    scan_dirs_where(dirs, blocked, &|_| true)
}

fn scan_dirs_where(dirs: &[PathBuf], blocked: &[PathBuf], allow: &dyn Fn(&Path) -> bool) -> Vec<Vst3Descriptor> {
    let mut out: Vec<Vst3Descriptor> = Vec::new();
    for path in paths::find_bundles(dirs) {
        if paths::is_blocked(&path, blocked) {
            log::info!("VST3 plugin {} is blocked: not loaded", path.display());
            continue;
        }
        if !allow(&path) {
            log::warn!("VST3 plugin {} is outside the allowed folders: not loaded", path.display());
            continue;
        }
        match load_bundle(&path) {
            Ok(ds) => {
                for d in ds {
                    if !out.iter().any(|o| o.id == d.id) {
                        out.push(d);
                    }
                }
            }
            Err(e) => log::warn!("{e}"),
        }
    }
    out.sort_by_key(|d| d.name.to_lowercase());
    out
}

fn ensure_scanned(r: &mut Registry) -> &Vec<Vst3Descriptor> {
    if r.scanned.is_none() {
        let dirs = search_dirs(r);
        let mut found = scan_dirs(&dirs, r.blocked());
        let targeted = r.targeted.clone();
        merge(&mut found, &|_| false, &targeted);
        r.scanned = Some(found);
    }
    r.scanned.get_or_insert_with(Vec::new)
}

pub fn scan() -> Vec<Vst3Descriptor> {
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    ensure_scanned(&mut r).clone()
}

pub fn cached() -> Option<Vec<Vst3Descriptor>> {
    registry().lock().unwrap_or_else(PoisonError::into_inner).scanned.clone()
}

pub fn rescan() -> Vec<Vst3Descriptor> {
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    r.scanned = None;
    r.failed.clear();
    r.targeted.clear();
    ensure_scanned(&mut r).clone()
}

pub fn add_search_dir(dir: &Path) -> usize {
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    if !r.extra_dirs.iter().any(|d| d == dir) {
        r.extra_dirs.push(dir.to_path_buf());
    }
    let found = scan_dirs(&[dir.to_path_buf()], r.blocked());
    let n = found.len();
    let list = r.scanned.get_or_insert_with(Vec::new);
    for d in found {
        if !list.iter().any(|o| o.id == d.id) {
            list.push(d);
        }
    }
    n
}

/// The folders a full scan reads: the standard ones and the environment's (see
/// [`paths::default_search_paths`]) and those added with [`add_search_dir`] or
/// [`register_search_dir`].
fn search_dirs(r: &Registry) -> Vec<PathBuf> {
    let mut dirs = paths::default_search_paths();
    dirs.extend(r.extra_dirs.iter().cloned());
    dirs
}

/// True when `path` lies in a folder a full scan reads, so loading it needs no new trust.
pub fn in_search_path(path: &Path) -> bool {
    let r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    search_dirs(&r).iter().any(|d| paths::is_inside(path, d))
}

/// Adds a folder to the search path without scanning it: the next full scan reads it, and
/// [`rescan_paths`] may target what it holds.
pub fn register_search_dir(dir: &Path) {
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    if !r.extra_dirs.iter().any(|d| d == dir) {
        r.extra_dirs.push(dir.to_path_buf());
    }
}

/// Rescans only `targets`: absolute bundle or folder paths inside the search path ([`in_search_path`]);
/// others are ignored, so a rescan never loads a binary a full scan wouldn't. Forgets the targets'
/// cached plugins and remembered failures, loads them again (unless blocked) and merges the result
/// into the cache, where it wins over another bundle with the same id. Returns what the targets hold
/// now. Before any full scan nothing else is loaded, and the plugins found can be created right away.
/// A binary loaded earlier in this process stays mapped, so a rebuilt plugin's new code needs a new
/// process; its descriptors are read again either way.
pub fn rescan_paths(targets: &[PathBuf]) -> Vec<Vst3Descriptor> {
    rescan_paths_where(targets, &|_| true)
}

/// [`rescan_paths`], loading only the bundles `allow` accepts (checked for every bundle found
/// below the targets, so a caller confining file access also covers linked subfolders).
pub fn rescan_paths_where(targets: &[PathBuf], allow: &dyn Fn(&Path) -> bool) -> Vec<Vst3Descriptor> {
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    let roots = search_dirs(&r);
    let targets: Vec<PathBuf> = targets
        .iter()
        .filter(|t| {
            let ok = t.is_absolute() && paths::entry_problem(t).is_none() && roots.iter().any(|d| paths::is_inside(t, d));
            if !ok {
                log::warn!("VST3 rescan: ignoring `{}`: not an absolute bundle or folder path in the search path", t.display());
            }
            ok
        })
        .cloned()
        .collect();
    let under = |p: &Path| targets.iter().any(|t| paths::is_under(p, t));
    let stale: Vec<String> = r.scanned.iter().flatten().chain(&r.targeted).filter(|d| under(&d.path)).map(|d| d.id.clone()).collect();
    let blocked = r.blocked().to_vec();
    let found = scan_dirs_where(&targets, &blocked, allow);
    for id in stale.iter().chain(found.iter().map(|d| &d.id)) {
        r.failed.remove(id);
    }
    merge(&mut r.targeted, &under, &found);
    if let Some(list) = r.scanned.as_mut() {
        merge(list, &under, &found);
    }
    found
}

/// Puts `found` into `list` in place of the plugins under the rescanned targets and of any other
/// bundle's plugin with the same id.
fn merge(list: &mut Vec<Vst3Descriptor>, under: &dyn Fn(&Path) -> bool, found: &[Vst3Descriptor]) {
    list.retain(|d| !under(&d.path) && !found.iter().any(|f| f.id == d.id));
    list.extend(found.iter().cloned());
    list.sort_by_key(|d| d.name.to_lowercase());
}

/// Replaces the blocklist ([`paths::is_blocked`] entries). Newly blocked plugins leave the cache
/// at once and can no longer be created (live instances keep running, and the parameter info of
/// a type already created stays available for them); unblocked ones come back with the next scan
/// of their folder ([`rescan`] or [`rescan_paths`]).
pub fn set_blocklist(entries: Vec<PathBuf>) {
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(list) = r.scanned.as_mut() {
        list.retain(|d| !paths::is_blocked(&d.path, &entries));
    }
    r.targeted.retain(|d| !paths::is_blocked(&d.path, &entries));
    r.blocked = Some(entries);
}

/// The current blocklist.
pub fn blocklist() -> Vec<PathBuf> {
    registry().lock().unwrap_or_else(PoisonError::into_inner).blocked().to_vec()
}

fn leak(s: String) -> &'static str {
    Box::leak(s.into_boxed_str())
}

/// Maps a VST3 unit label to a SoundCraft display unit.
pub(crate) fn unit_from_label(units: &str) -> Unit {
    match units.trim().to_ascii_lowercase().as_str() {
        "db" => Unit::Db,
        "hz" => Unit::Hz,
        "ms" => Unit::Ms,
        "%" => Unit::Percent,
        "s" | "sec" | "secs" | "seconds" => Unit::Seconds,
        "st" | "semi" | "semitones" => Unit::Semitones,
        "ct" | "cent" | "cents" => Unit::Cents,
        _ => Unit::None,
    }
}

fn param_info(inst: &Instance, p: &RawParam) -> ParamInfo {
    let name = if p.title.is_empty() { format!("Param {}", p.id) } else { p.title.clone() };
    let base = ParamInfo {
        id: leak(p.id.to_string()),
        name: leak(name),
        min: 0.0,
        max: 1.0,
        default: 0.0,
        unit: Unit::None,
        taper: Taper::Linear,
        choices: &[],
    };
    if p.step_count == 1 {
        return ParamInfo { unit: Unit::Toggle, default: p.default_normalized.round() as f32, ..base };
    }
    if (2..=MAX_CHOICES).contains(&p.step_count) {
        let steps = f64::from(p.step_count);
        let labels: Vec<&'static str> =
            (0..=p.step_count).map(|i| leak(inst.value_text(p.id, f64::from(i) / steps).unwrap_or_else(|| i.to_string()))).collect();
        return ParamInfo {
            max: p.step_count as f32,
            default: (p.default_normalized * steps).round() as f32,
            unit: Unit::Choice,
            choices: Box::leak(labels.into_boxed_slice()),
            ..base
        };
    }
    // Continuous (or too many steps for a menu): plain range from the controller.
    let unit = unit_from_label(&p.units);
    match (inst.to_plain(p.id, 0.0), inst.to_plain(p.id, 1.0)) {
        (Some(lo), Some(hi)) if hi > lo && (lo as f32).is_finite() && (hi as f32).is_finite() && (hi as f32) > (lo as f32) => {
            let default = inst.to_plain(p.id, p.default_normalized).map_or(lo, |d| d.clamp(lo, hi));
            // A logarithmic plugin mapping (frequency knobs) shows its geometric mean at mid travel.
            let taper = match inst.to_plain(p.id, 0.5) {
                Some(mid)
                    if lo > 0.0
                        && ((mid - (lo * hi).sqrt()).abs() <= 0.01 * (lo * hi).sqrt())
                        && (mid - (lo + hi) / 2.0).abs() > 0.01 * (hi - lo) =>
                {
                    Taper::Log
                }
                _ => Taper::Linear,
            };
            ParamInfo { min: lo as f32, max: hi as f32, default: default as f32, unit, taper, ..base }
        }
        _ => ParamInfo { default: p.default_normalized as f32, unit, ..base },
    }
}

/// Builds and leaks the `PluginInfo` for one plugin type. Called at most once per id (cached).
fn build_info(d: &Vst3Descriptor, inst: &Instance) -> &'static PluginInfo {
    let params: Vec<ParamInfo> =
        inst.params().iter().filter(|p| p.flags & (PARAM_HIDDEN | PARAM_READ_ONLY | PARAM_BYPASS) == 0).map(|p| param_info(inst, p)).collect();
    let short: String = d.name.chars().take(8).collect();
    Box::leak(Box::new(PluginInfo {
        id: leak(d.id.clone()),
        name: leak(d.name.clone()),
        short_name: leak(short),
        category: d.category,
        params: Box::leak(params.into_boxed_slice()),
        is_instrument: d.is_instrument,
        audiosuite: false,
    }))
}

/// Finds the descriptor, loads its binary and creates an instance plus its (cached) info.
fn instantiate_raw(id: &str) -> Result<(Instance, &'static PluginInfo), Vst3Error> {
    let cid = parse_id(id).ok_or_else(|| Vst3Error::NotFound(id.to_string()))?;
    let desc = {
        let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if r.failed.contains(id) {
            return Err(Vst3Error::Instantiate(id.to_string(), "failed before".into()));
        }
        let known = r.scanned.iter().flatten().chain(&r.targeted).find(|d| d.id == id).cloned();
        let desc = match known {
            Some(d) => d,
            None => ensure_scanned(&mut r).iter().find(|d| d.id == id).cloned().ok_or_else(|| Vst3Error::NotFound(id.to_string()))?,
        };
        if paths::is_blocked(&desc.path, r.blocked()) {
            return Err(Vst3Error::Instantiate(id.to_string(), "blocked".into()));
        }
        desc
    };
    let made = Bundle::load(&desc.path).and_then(|b| b.instantiate(&cid));
    let mut r = registry().lock().unwrap_or_else(PoisonError::into_inner);
    // Blocked while it loaded: the new instance is dropped.
    if paths::is_blocked(&desc.path, r.blocked()) {
        return Err(Vst3Error::Instantiate(id.to_string(), "blocked".into()));
    }
    match made {
        Ok(inst) => {
            let info = match r.infos.get(id) {
                Some(i) => *i,
                None => {
                    let i = build_info(&desc, &inst);
                    r.infos.insert(id.to_string(), i);
                    i
                }
            };
            Ok((inst, info))
        }
        Err(e) => {
            r.failed.insert(id.to_string());
            Err(e)
        }
    }
}

pub fn instantiate(id: &str) -> Result<Box<dyn Plugin>, Vst3Error> {
    Ok(Box::new(instantiate_plugin(id)?))
}

pub fn instantiate_plugin(id: &str) -> Result<Vst3Plugin, Vst3Error> {
    let (inst, info) = instantiate_raw(id)?;
    let mut p = Vst3Plugin::new(inst, info);
    p.prepare(48_000.0, 1024, 2);
    Ok(p)
}

pub fn plugin_info(id: &str) -> Option<&'static PluginInfo> {
    parse_id(id)?;
    {
        let r = registry().lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(i) = r.infos.get(id) {
            return Some(*i);
        }
        if r.failed.contains(id) {
            return None;
        }
    }
    // Instantiate once to learn the parameters (VST3 exposes them only on a live controller).
    instantiate_raw(id).ok().map(|(_, info)| info)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unit_labels_map() {
        assert_eq!(unit_from_label("dB"), Unit::Db);
        assert_eq!(unit_from_label(" Hz "), Unit::Hz);
        assert_eq!(unit_from_label("%"), Unit::Percent);
        assert_eq!(unit_from_label("ms"), Unit::Ms);
        assert_eq!(unit_from_label("sec"), Unit::Seconds);
        assert_eq!(unit_from_label("cents"), Unit::Cents);
        assert_eq!(unit_from_label("st"), Unit::Semitones);
        assert_eq!(unit_from_label("furlongs"), Unit::None);
    }
}
