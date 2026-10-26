//! The host bridge's decisions, kept free of `web_sys` so they are tested natively: which parent
//! origin the bridge may talk to, and which saves the parent has confirmed.

use std::collections::BTreeMap;

/// How long the parent has to answer `soundcraft:save` with `soundcraft:saved` (milliseconds).
pub const SAVE_ACK_TIMEOUT_MS: f64 = 30_000.0;

/// The origin the bridge posts to and hears from, or `None` for no bridge (saves download).
///
/// - `own`: this page's origin.
/// - `parent`: the parent window's origin, `None` when reading it threw (a cross-origin parent).
/// - `allowed`: the `?host_origin=` query value, an origin the embedding page vouches for.
///
/// A same-origin parent is accepted. A cross-origin parent is accepted only when `allowed` is a
/// valid URL origin (`scheme://host[:port]`, nothing else); messages then go to that exact origin,
/// so the browser drops them if the parent is anything else. Opaque origins (`"null"`) never match.
pub fn target_origin(own: &str, parent: Option<&str>, allowed: Option<&str>) -> Option<String> {
    if let Some(parent) = parent
        && parent == own
        && is_origin(own)
    {
        return Some(own.to_string());
    }
    let allowed = allowed?.trim();
    is_origin(allowed).then(|| allowed.to_string())
}

/// True for a serialised URL origin: `http(s)://host[:port]` with no path, query, fragment or
/// credentials.
pub fn is_origin(s: &str) -> bool {
    let Some((scheme, rest)) = s.split_once("://") else { return false };
    if !matches!(scheme, "http" | "https") || rest.is_empty() {
        return false;
    }
    // IPv6 literals aren't accepted; name the host instead.
    if rest.contains(['/', '?', '#', '@', ' ', '\\', '[', ']']) {
        return false;
    }
    let (host, port) = match rest.split_once(':') {
        Some((h, p)) => (h, Some(p)),
        None => (rest, None),
    };
    if host.is_empty() {
        return false;
    }
    match port {
        Some(p) => p.bytes().all(|b| b.is_ascii_digit()) && p.parse::<u16>().is_ok(),
        None => true,
    }
}

/// Saves posted to the parent and not yet confirmed.
#[derive(Debug, Default)]
pub struct SaveAcks {
    next_id: u64,
    /// Request id → (file name, deadline in ms, whether it is the session itself).
    pending: BTreeMap<u64, (String, f64, bool)>,
    /// A session save failed or went unanswered: the session stays dirty until one is confirmed.
    unconfirmed: bool,
}

impl SaveAcks {
    /// Record a save posted at `now`; returns its request id.
    pub fn posted(&mut self, name: &str, session: bool, now: f64) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.insert(self.next_id, (name.to_string(), now + SAVE_ACK_TIMEOUT_MS, session));
        self.next_id
    }

    /// The parent confirmed request `id`. False when no such save is waiting.
    pub fn saved(&mut self, id: u64) -> bool {
        match self.pending.remove(&id) {
            Some((_, _, session)) => {
                if session {
                    self.unconfirmed = false;
                }
                true
            }
            None => false,
        }
    }

    /// The parent reported request `id` failed; returns its name.
    pub fn failed(&mut self, id: u64) -> Option<String> {
        let (name, _, session) = self.pending.remove(&id)?;
        self.unconfirmed |= session;
        Some(name)
    }

    /// Saves past their deadline at `now`, removed and returned as `(id, name)`.
    pub fn expire(&mut self, now: f64) -> Vec<(u64, String)> {
        let late: Vec<u64> = self.pending.iter().filter(|(_, (_, deadline, _))| now >= *deadline).map(|(id, _)| *id).collect();
        late.into_iter()
            .filter_map(|id| {
                let (name, _, session) = self.pending.remove(&id)?;
                self.unconfirmed |= session;
                Some((id, name))
            })
            .collect()
    }

    /// Whether any save is still waiting for the parent.
    pub fn waiting(&self) -> bool {
        !self.pending.is_empty()
    }

    /// The unsaved-changes state to report: the engine's, held dirty while a session save is
    /// unconfirmed.
    pub fn dirty(&self, engine_dirty: bool) -> bool {
        engine_dirty || self.unconfirmed || self.pending.values().any(|(_, _, session)| *session)
    }
}

/// Refuse `soundcraft:open` bytes (`len`) when they, plus the `queued` bytes still waiting for the
/// next frame, exceed `cap`. Checked before the bytes are copied; draining the queue frees room.
pub fn check_open(name: &str, queued: f64, len: f64, cap: u64) -> Result<(), String> {
    if queued + len > cap as f64 {
        return Err(format!("{name}: file is too large to open ({len} bytes, {queued} already waiting; the limit is {cap} bytes)"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const OWN: &str = "https://app.example";

    #[test]
    fn same_origin_parent_enables_the_bridge() {
        assert_eq!(target_origin(OWN, Some(OWN), None).as_deref(), Some(OWN));
    }

    #[test]
    fn cross_origin_parent_disables_the_bridge() {
        // Reading a cross-origin parent's origin throws (None) or names another origin.
        assert_eq!(target_origin(OWN, None, None), None);
        assert_eq!(target_origin(OWN, Some("https://evil.example"), None), None);
        assert_eq!(target_origin(OWN, Some("null"), None), None);
        assert_eq!(target_origin("null", Some("null"), None), None);
    }

    #[test]
    fn host_origin_parameter_must_be_an_exact_origin() {
        let files = "https://files.example:8443";
        assert_eq!(target_origin(OWN, None, Some(files)).as_deref(), Some(files));
        for bad in
            ["*", "null", "files.example", "https://files.example/", "https://a@files.example", "javascript://x", "https://", "https://x:99999"]
        {
            assert_eq!(target_origin(OWN, None, Some(bad)), None, "{bad}");
        }
    }

    #[test]
    fn save_stays_dirty_until_the_parent_confirms() {
        let mut acks = SaveAcks::default();
        let id = acks.posted("a.scraftz", true, 0.0);
        // The engine marked the session saved, but the parent hasn't confirmed.
        assert!(acks.dirty(false));
        assert!(!acks.saved(id + 1), "an unknown id confirms nothing");
        assert!(acks.dirty(false));
        assert!(acks.saved(id));
        assert!(!acks.dirty(false));
        assert!(!acks.waiting());
    }

    #[test]
    fn unanswered_save_times_out_as_failed_and_stays_dirty() {
        let mut acks = SaveAcks::default();
        let id = acks.posted("a.scraftz", true, 1_000.0);
        assert!(acks.expire(1_000.0 + SAVE_ACK_TIMEOUT_MS - 1.0).is_empty());
        assert_eq!(acks.expire(1_000.0 + SAVE_ACK_TIMEOUT_MS), vec![(id, "a.scraftz".to_string())]);
        assert!(acks.dirty(false));
        // A later confirmed save clears it.
        let next = acks.posted("a.scraftz", true, 50_000.0);
        assert!(acks.saved(next));
        assert!(!acks.dirty(false));
    }

    #[test]
    fn export_acks_do_not_hold_the_session_dirty() {
        let mut acks = SaveAcks::default();
        let id = acks.posted("mix.wav", false, 0.0);
        assert!(!acks.dirty(false));
        assert_eq!(acks.failed(id).as_deref(), Some("mix.wav"));
        assert!(!acks.dirty(false));
    }

    #[test]
    fn open_cap_counts_documents_still_waiting() {
        assert!(check_open("a.scraftz", 0.0, 100.0, 100).is_ok());
        let e = check_open("a.scraftz", 0.0, 101.0, 100).unwrap_err();
        assert!(e.contains("too large") && e.contains("a.scraftz"));
        // Two documents that each fit are refused together until the first is drained.
        assert!(check_open("b.mid", 60.0, 60.0, 100).is_err());
        assert!(check_open("b.mid", 0.0, 60.0, 100).is_ok());
    }
}
