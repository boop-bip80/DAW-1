//! Native, incremental stem export. A job owns its same-filesystem staging directory until the
//! entire batch has been published without replacing any existing destination.
//!
//! Each call to `step` renders at least one block or publishes one completed file, then honours
//! the caller's time budget. Dropping an unfinished job cancels it: the active bounce removes its
//! owned sidecar and `StemBatch` rolls back published links and its private staging directory.

use crate::io::{self, Bounce, StemBatch};
use crate::{Engine, EngineError, Result};
use soundcraft_audio_io::{BitDepth, EncodeOptions, FileFormat};
use soundcraft_model::{Session, TrackId, TrackKind};
use soundcraft_time::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A native stem bounce that can be advanced for a bounded amount of UI time and cancelled by
/// dropping it. The session is a frozen snapshot; no document or command journal is mutated.
pub struct StemBounceJob {
    session: Arc<Session>,
    range: Range,
    options: EncodeOptions,
    fold_stereo: bool,
    destination_dir: PathBuf,
    stems: Vec<(TrackId, String)>,
    files: Vec<PathBuf>,
    paths: Vec<String>,
    batch: Option<StemBatch>,
    active: Option<Bounce>,
    rendered: usize,
    published: usize,
    final_preflight: bool,
    finished: bool,
    failed: bool,
}

impl StemBounceJob {
    /// Preflight every final name before making an output directory. Audio, instrument, and
    /// MIDI tracks that are active and not muted each become one stem, preserving the existing
    /// `file.bounce_stems` selection and naming semantics.
    pub fn new(e: &Engine, dir: &str, range: Range, options: &EncodeOptions, fold_stereo: bool) -> Result<Self> {
        let session = e.session_arc();
        if range.len() > session.sample_rate.samples(io::MAX_BOUNCE_SECS) {
            return Err(EngineError::BadParams("file.bounce_stems".into(), "bounces are limited to 4 hours".into()));
        }
        let ext = match options.format {
            FileFormat::Aiff => "aif",
            FileFormat::Flac => "flac",
            _ => "wav",
        };
        let stems: Vec<(TrackId, String)> = session
            .tracks
            .iter()
            .filter(|track| matches!(track.kind, TrackKind::Audio | TrackKind::Instrument | TrackKind::Midi) && !track.inactive && !track.mixer.mute)
            .map(|track| (track.id, track.name.clone()))
            .collect();
        if stems.len() > 4_096 {
            return Err(EngineError::BadParams("file.bounce_stems".into(), "stem export is limited to 4096 tracks".into()));
        }
        let names = io::stem_output_names(&session.name, &stems, ext)?;
        let destination_dir = PathBuf::from(dir);
        let files: Vec<PathBuf> = names.iter().map(|name| destination_dir.join(name)).collect();
        let paths: Vec<String> = files.iter().map(|path| path.to_string_lossy().into_owned()).collect();
        let batch = if stems.is_empty() {
            None
        } else {
            io::preflight_stem_paths(&files)?;
            io::make_dir(dir)?;
            Some(StemBatch::reserve(&destination_dir)?)
        };
        Ok(Self {
            session,
            range,
            options: options.clone(),
            fold_stereo,
            destination_dir,
            stems,
            files,
            paths,
            batch,
            active: None,
            rendered: 0,
            published: 0,
            final_preflight: false,
            finished: false,
            failed: false,
        })
    }

    /// Completed renders and current render progress consume 95% of the progress bar; the final
    /// no-replace publication of the batch consumes the remaining 5%.
    pub fn progress(&self) -> f32 {
        if self.finished || self.stems.is_empty() {
            return 1.0;
        }
        let n = self.stems.len() as f32;
        let current = self.active.as_ref().map_or(0.0, Bounce::progress);
        let rendering = 0.95 * ((self.rendered as f32 + current) / n);
        if self.rendered < self.stems.len() { rendering } else { rendering + 0.05 * self.published as f32 / n }
    }

    pub fn completed_stems(&self) -> usize {
        self.rendered
    }

    pub fn total_stems(&self) -> usize {
        self.stems.len()
    }

    /// The track currently rendering, or `None` during publication or after completion.
    pub fn current_stem(&self) -> Option<&str> {
        self.stems.get(self.rendered).map(|(_, name)| name.as_str())
    }

    /// Render/publish a bounded slice. `more` is checked after each audio block and each final
    /// hard link; even an always-false budget advances at least one unit before yielding.
    /// `Some(paths)` means the entire batch was committed successfully.
    pub fn step(&mut self, more: impl FnMut() -> bool) -> Result<Option<Vec<String>>> {
        self.step_with_publisher(more, io::publish_stem_link_no_replace)
    }

    /// Record a completed GUI export without running `file.bounce_stems` a second time.
    /// Consuming the job prevents duplicate journal entries. Unfinished or failed jobs cannot
    /// claim success; dropping them still rolls back their owned staging and published links.
    pub fn record_completed(mut self, engine: &mut Engine) -> Result<Vec<String>> {
        if !self.finished || self.failed {
            return Err(EngineError::Io("cannot journal an unfinished stem export".into()));
        }
        if !Arc::ptr_eq(&self.session, &engine.session_arc()) {
            return Err(EngineError::BadParams(
                "file.bounce_stems".into(),
                "the session changed during stem export; completed files remain but cannot be journaled to a different session".into(),
            ));
        }
        let format = match self.options.format {
            FileFormat::Wav => "wav",
            FileFormat::Aiff => "aiff",
            FileFormat::Flac => "flac",
            _ => return Err(EngineError::BadParams("file.bounce_stems".into(), "unsupported completed stem file format".into())),
        };
        let bit_depth = match self.options.bit_depth {
            BitDepth::Int16 => "16",
            BitDepth::Int24 => "24",
            BitDepth::Float32 => "32f",
            _ => return Err(EngineError::BadParams("file.bounce_stems".into(), "unsupported completed stem bit depth".into())),
        };
        let params = serde_json::json!({
            "dir": self.destination_dir.to_string_lossy(),
            "start": self.range.start,
            "end": self.range.end,
            "format": format,
            "bit_depth": bit_depth,
            "dither": self.options.dither,
            "fold_down": if self.fold_stereo { "stereo" } else { "none" },
        });
        engine.journal.push(("file.bounce_stems".into(), params));
        Ok(std::mem::take(&mut self.paths))
    }

    /// Publisher injection preserves the synchronous command's existing race/rollback tests.
    /// Production always uses `std::fs::hard_link` (atomic no-replace publication).
    pub(crate) fn step_with_publisher(
        &mut self,
        mut more: impl FnMut() -> bool,
        mut publish: impl FnMut(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<Option<Vec<String>>> {
        if self.failed {
            return Err(EngineError::Io("stem bounce has already failed".into()));
        }
        let result = self.advance(&mut more, &mut publish);
        if result.is_err() {
            // Errors are terminal. Immediately remove owned sidecars, staged files, and any
            // already-published links, even if a caller retains the failed job.
            self.active = None;
            self.batch = None;
            self.failed = true;
        }
        result
    }

    fn advance(
        &mut self,
        more: &mut impl FnMut() -> bool,
        publish: &mut impl FnMut(&Path, &Path) -> std::io::Result<()>,
    ) -> Result<Option<Vec<String>>> {
        loop {
            if self.finished {
                return Ok(Some(self.paths.clone()));
            }
            if self.rendered < self.stems.len() {
                if self.active.is_none() {
                    self.begin_stem()?;
                }
                let Some(active) = self.active.as_mut() else {
                    return Err(EngineError::Io("stem bounce lost its active render".into()));
                };
                if active.step(&mut *more)?.is_none() {
                    return Ok(None);
                }
                self.active = None;
                self.rendered += 1;
                if !more() {
                    return Ok(None);
                }
                continue;
            }

            if !self.final_preflight {
                // An unrelated process may have occupied a destination while stems rendered.
                io::preflight_stem_paths(&self.files)?;
                self.final_preflight = true;
            }
            if self.published < self.files.len() {
                let Some(batch) = self.batch.as_mut() else {
                    return Err(EngineError::Io("stem bounce lost its staging directory".into()));
                };
                let Some(destination) = self.files.get(self.published) else {
                    return Err(EngineError::Io("stem bounce lost its destination".into()));
                };
                batch.publish_one(destination, publish)?;
                self.published += 1;
                if !more() {
                    return Ok(None);
                }
                continue;
            }

            if let Some(batch) = self.batch.as_mut() {
                batch.commit(&self.destination_dir);
            }
            // A committed batch still owns its private copies, not the hard-linked outputs.
            // Drop its staging now instead of keeping duplicate files until the UI drops us.
            self.batch = None;
            self.finished = true;
            return Ok(Some(self.paths.clone()));
        }
    }

    fn begin_stem(&mut self) -> Result<()> {
        let Some((track_id, _)) = self.stems.get(self.rendered) else {
            return Err(EngineError::Io("stem bounce lost its track".into()));
        };
        let Some(destination) = self.files.get(self.rendered) else {
            return Err(EngineError::Io("stem bounce lost its destination".into()));
        };
        let Some(batch) = self.batch.as_ref() else {
            return Err(EngineError::Io("stem bounce lost its staging directory".into()));
        };
        let staged = batch.staged_path(destination)?;
        let mut solo = (*self.session).clone();
        for track in &mut solo.tracks {
            track.mixer.solo = track.id == *track_id;
        }
        self.active = Some(Bounce::new(Arc::new(solo), &staged, self.range, &self.options, None, false, self.fold_stereo)?);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use soundcraft_model::{ChannelFormat, TrackKind};
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_DIR_ID: AtomicU64 = AtomicU64::new(0);

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(label: &str) -> Self {
            let id = TEST_DIR_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!("soundcraft-stem-incremental-{}-{id}-{label}", std::process::id()));
            std::fs::create_dir(&path).expect("unique test export directory");
            Self(path)
        }

        fn as_str(&self) -> String {
            self.0.to_string_lossy().into_owned()
        }

        fn entries(&self) -> Vec<String> {
            std::fs::read_dir(&self.0)
                .expect("read export directory")
                .map(|entry| entry.expect("directory entry").file_name().to_string_lossy().into_owned())
                .collect()
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn engine() -> Engine {
        let mut engine = Engine::default();
        engine.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, Some("Guitar"));
        engine.session_mut().add_track(TrackKind::Instrument, ChannelFormat::Stereo, Some("Keys"));
        engine.mark_clean();
        engine
    }

    fn start(engine: &Engine, dir: &TestDir) -> StemBounceJob {
        StemBounceJob::new(engine, &dir.as_str(), Range::new(0, 4_800), &EncodeOptions::default(), false).expect("preflight job")
    }

    #[test]
    fn cancellation_after_partial_render_removes_only_owned_staging_and_keeps_session_clean() {
        let dir = TestDir::new("cancel-render");
        let keep = dir.0.join("keep.txt");
        std::fs::write(&keep, b"not part of the export").expect("write sentinel");
        let engine = engine();
        let undo_before = engine.undo_history().len();
        let journal_before = engine.journal.len();
        let mut job = start(&engine, &dir);
        assert_eq!(job.total_stems(), 2);
        assert_eq!(job.completed_stems(), 0);
        assert_eq!(job.current_stem(), Some("Guitar"));
        assert!(job.step(|| false).expect("first render slice").is_none());
        assert!(job.progress() > 0.0 && job.progress() < 0.95);
        assert!(dir.entries().iter().any(|name| name.starts_with(".scstems-")), "job must own a private staging directory");
        drop(job);
        assert_eq!(dir.entries(), vec!["keep.txt".to_owned()], "cancel must not leave staged, .part, or published stem files");
        assert_eq!(std::fs::read(&keep).expect("sentinel survives"), b"not part of the export");
        assert!(!engine.is_dirty());
        assert_eq!(engine.undo_history().len(), undo_before);
        assert_eq!(engine.journal.len(), journal_before);
    }

    #[test]
    fn one_unit_slices_progress_monotonically_and_publish_all_stems_without_overwrite() {
        let dir = TestDir::new("progress");
        let engine = engine();
        let mut job = start(&engine, &dir);
        let mut prior = job.progress();
        let mut slices = 0usize;
        let paths = loop {
            slices += 1;
            assert!(slices < 80, "bounded job did not finish");
            let result = job.step(|| false).expect("advance one unit");
            let now = job.progress();
            assert!(now + f32::EPSILON >= prior, "progress regressed from {prior} to {now}");
            assert!(now <= 1.0);
            prior = now;
            if let Some(paths) = result {
                break paths;
            }
        };
        assert_eq!(paths.len(), 2);
        assert_eq!(job.progress(), 1.0);
        assert!(job.current_stem().is_none());
        for path in &paths {
            assert!(Path::new(path).is_file(), "{path}: complete stem published");
            assert!(!PathBuf::from(format!("{path}.part")).exists(), "no sidecar left at {path}");
        }
        assert!(!dir.entries().iter().any(|name| name.starts_with(".scstems-")), "committed staging directory must be removed immediately");
        assert!(!engine.is_dirty(), "offline rendering a snapshot cannot modify the session");
        drop(job);
        assert!(paths.iter().all(|path| Path::new(path).is_file()), "dropping a committed job must not delete completed stems");
    }

    #[test]
    fn dropping_after_first_publication_rolls_back_completed_links() {
        let dir = TestDir::new("cancel-publish");
        let engine = engine();
        let mut job = start(&engine, &dir);
        while job.completed_stems() < job.total_stems() {
            assert!(job.step(|| false).expect("render step").is_none());
        }
        assert!(job.step(|| false).expect("first publication").is_none());
        assert_eq!(job.published, 1);
        assert!(job.files[0].is_file());
        drop(job);
        assert!(dir.entries().is_empty(), "cancelled publication must roll back its links and staging");
    }

    #[test]
    fn occupied_path_appearing_after_render_is_never_replaced_and_cleans_staging() {
        let dir = TestDir::new("race");
        let engine = engine();
        let mut job = start(&engine, &dir);
        let occupied = job.files[1].clone();
        assert!(job.step(|| false).expect("first slice").is_none());
        std::fs::write(&occupied, b"foreign output").expect("race fixture");
        let error = loop {
            match job.step(|| false) {
                Ok(None) => continue,
                Ok(Some(_)) => panic!("export unexpectedly overwrote an occupied path"),
                Err(error) => break error,
            }
        };
        assert!(error.to_string().contains("already exists"), "{error}");
        assert_eq!(std::fs::read(&occupied).expect("foreign file survives"), b"foreign output");
        assert_eq!(dir.entries().len(), 1, "failed render must clean staging immediately");
        assert!(job.step(|| true).is_err(), "a failed job must not retry and publish");
    }

    #[test]
    fn failed_second_publication_rolls_back_first_link_but_not_external_file() {
        let dir = TestDir::new("publish-failure");
        let engine = engine();
        let mut job = start(&engine, &dir);
        let files = job.files.clone();
        let mut calls = 0;
        let error = loop {
            let result = job.step_with_publisher(
                || false,
                |from, to| {
                    calls += 1;
                    if calls == 2 {
                        std::fs::write(to, b"foreign").expect("interleaved file");
                        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "interleaved target"));
                    }
                    std::fs::hard_link(from, to)
                },
            );
            match result {
                Ok(None) => continue,
                Ok(Some(_)) => panic!("failed publication was accepted"),
                Err(error) => break error,
            }
        };
        assert!(error.to_string().contains("cannot publish stem"), "{error}");
        assert!(!files[0].exists(), "first link was owned by the failed batch and must roll back");
        assert_eq!(std::fs::read(&files[1]).expect("foreign file survives"), b"foreign");
        assert_eq!(dir.entries().len(), 1, "failed batch must not leave its stage");
    }

    #[test]
    fn synchronous_command_uses_same_job_and_preserves_noundo_journal_semantics() {
        let dir = TestDir::new("command");
        let mut engine = engine();
        let journal_before = engine.journal.len();
        let undo_before = engine.undo_history().len();
        let result = engine.execute("file.bounce_stems", &serde_json::json!({"dir": dir.as_str(), "start": 0, "end": 4_800})).expect("sync command");
        assert_eq!(result["files"].as_array().expect("output paths").len(), 2);
        assert!(!engine.is_dirty(), "offline export is not a document edit");
        assert_eq!(engine.undo_history().len(), undo_before, "noundo command creates no undo entry");
        assert_eq!(engine.journal.len(), journal_before + 1, "noundo command is still journaled");
        assert_eq!(engine.journal.last().map(|(id, _)| id.as_str()), Some("file.bounce_stems"));
    }

    #[test]
    fn gui_completion_requires_committed_batch_and_journals_normalized_export_once() {
        let dir = TestDir::new("gui-journal");
        let mut engine = engine();
        let undo_before = engine.undo_history().len();
        let mut unfinished = start(&engine, &dir);
        assert!(unfinished.step(|| false).expect("partial render").is_none());
        assert!(unfinished.record_completed(&mut engine).is_err(), "an unfinished export cannot claim success");
        assert!(engine.journal.is_empty(), "cancellation must not add a journal entry");
        assert!(dir.entries().is_empty(), "unfinished job still rolls back staging on drop");

        let mut job = start(&engine, &dir);
        loop {
            if job.step(|| false).expect("bounded render").is_some() {
                break;
            }
        }
        let files = job.record_completed(&mut engine).expect("committed batch may be journaled");
        assert_eq!(files.len(), 2);
        assert!(files.iter().all(|path| Path::new(path).is_file()), "completed outputs persist");
        assert_eq!(engine.journal.len(), 1, "one export records one event");
        assert_eq!(
            engine.journal[0],
            (
                "file.bounce_stems".to_owned(),
                serde_json::json!({
                    "dir": dir.as_str(), "start": 0, "end": 4_800, "format": "wav",
                    "bit_depth": "24", "dither": false, "fold_down": "none"
                }),
            )
        );
        assert!(!engine.is_dirty(), "export must not edit the session");
        assert_eq!(engine.undo_history().len(), undo_before, "completed export is not undoable");
    }

    #[test]
    fn completed_export_cannot_be_journaled_into_another_or_replaced_session() {
        let dir = TestDir::new("foreign-engine");
        let origin = engine();
        let mut job = start(&origin, &dir);
        loop {
            if job.step(|| false).expect("complete export").is_some() {
                break;
            }
        }
        let mut other = engine();
        let error = job.record_completed(&mut other).expect_err("another engine must not claim the finished export");
        assert!(error.to_string().contains("session changed"), "{error}");
        assert!(other.journal.is_empty(), "different engine cannot receive a fabricated completion event");
        assert!(origin.journal.is_empty(), "the source session cannot be journaled by a different engine");
        assert_eq!(dir.entries().len(), 2, "already committed files are not deleted on journal rejection");

        let replaced_dir = TestDir::new("replaced-engine");
        let mut engine = engine();
        let mut job = start(&engine, &replaced_dir);
        loop {
            if job.step(|| false).expect("complete export").is_some() {
                break;
            }
        }
        engine.replace_session(Session::default());
        let error = job.record_completed(&mut engine).expect_err("replaced session must refuse completed export journal");
        assert!(error.to_string().contains("session changed"), "{error}");
        assert!(engine.journal.is_empty());
        assert_eq!(replaced_dir.entries().len(), 2, "finished files survive replacement without mis-journaling");
    }
}
