//! Stem separation command for SoundCraft

use super::*;
use crate::cmd;
use serde_json::json;
use soundcraft_model::ClipContent;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

pub fn specs() -> Vec<CommandSpec> {
    vec![cmd!(
        "audiosuite.stem_separation",
        "Stem Separation",
        [],
        None,
        "{model_path?: <path to HTDemucs ONNX model>, output_dir?: <output directory for stems>} — separates selected audio clips into 4 stems (drums, bass, other, vocals)",
        has_selection,
        stem_separation
    )]
}

fn stem_separation(e: &mut Engine, p: &Value) -> Result<Value> {
    let ids = clip_ids_param(e, p);
    if ids.is_empty() {
        return Err(bad("audiosuite.stem_separation", "select audio clips first"));
    }

    let model_path = str_param(p, "model_path")
        .unwrap_or("models/htdemucs.onnx")
        .to_string();
    let output_dir = str_param(p, "output_dir")
        .unwrap_or("stems_output")
        .to_string();

    // Model will be auto-downloaded if it doesn't exist
    let mut results = Vec::new();
    let mut model_name = String::new();
    let mut created_tracks = Vec::new();

    for id in ids {
        let Some((track_id, clip)) = e.session().find_clip(id).map(|(t, c)| (t, c.clone())) else {
            continue;
        };

        if !clip.is_audio() {
            continue;
        }

        // Get the track name for stem track naming
        let track_name = e.session().track(track_id).map(|t| t.name.clone()).unwrap_or_else(|| "Audio".to_string());

        // Create output directory
        std::fs::create_dir_all(&output_dir).map_err(|e| bad("audiosuite.stem_separation", format!("failed to create output directory: {}", e)))?;

        // Get the actual audio file path from the clip
        let audio_path = match &clip.content {
            ClipContent::Audio { source, .. } => {
                log::info!("Clip source ID: {:?}", source);
                log::info!("Session folder: {:?}", e.session().folder);
                
                // Get the source from the session
                let source = e.session().source(*source);
                log::info!("Source from session: {:?}", source.as_ref().map(|s| &s.path));
                
                match (source, e.session().folder.as_ref()) {
                    (Some(src), Some(folder)) => {
                        let path = folder.join(&src.path);
                        log::info!("Full audio path: {:?}", path);
                        path.to_str()
                            .map(|s| s.to_string())
                            .ok_or_else(|| bad("audiosuite.stem_separation", "could not convert path to string"))
                    }
                    (Some(src), None) => {
                        // Fallback: try to use the source path directly if it's absolute
                        log::info!("No session folder, trying source path directly: {:?}", src.path);
                        if src.path.starts_with("/") || src.path.contains(":") {
                            Ok(src.path.clone())
                        } else {
                            Err(bad("audiosuite.stem_separation", "session folder not set and source path is relative"))
                        }
                    }
                    (None, _) => Err(bad("audiosuite.stem_separation", "source not found in session")),
                }
            }
            _ => return Err(bad("audiosuite.stem_separation", "clip is not an audio clip")),
        }?;

        log::info!("Starting stem separation for clip: {}", clip.name);
        
        // Run stem separation in background thread with progress updates
        let (progress_tx, progress_rx) = mpsc::channel::<(f32, String)>();
        let audio_path_clone = audio_path.clone();
        let output_dir_clone = output_dir.clone();
        let model_path_clone = model_path.clone();
        
        let handle = thread::spawn(move || {
            let progress_cb = std::sync::Arc::new(std::sync::Mutex::new(move |pct: f32, msg: &str| {
                let _ = progress_tx.send((pct, msg.to_string()));
            }));
            
            soundcraft_dsp::stem_separation::separate_audio(
                &audio_path_clone,
                &output_dir_clone,
                &model_path_clone,
                Some(progress_cb),
            )
        });
        
        // Poll for progress (non-blocking)
        loop {
            // Check for progress updates
            match progress_rx.try_recv() {
                Ok((pct, msg)) => {
                    log::info!("Stem separation progress: {:.0}% - {}", pct * 100.0, msg);
                }
                Err(mpsc::TryRecvError::Empty) => {
                    // No update yet
                }
                Err(mpsc::TryRecvError::Disconnected) => {
                    // Thread finished
                    break;
                }
            }
            
            // Check if thread is done
            if handle.is_finished() {
                break;
            }
            
            // Small sleep to avoid busy-waiting
            thread::sleep(Duration::from_millis(100));
        }
        
        // Get the result
        let result = handle.join()
            .map_err(|e| bad("audiosuite.stem_separation", format!("stem separation thread panicked: {:?}", e)))?;
        
        let result = result.map_err(|e| bad("audiosuite.stem_separation", format!("separation failed: {}", e)))?;

        model_name = result.model.clone();

        // Create a new track for each stem
        for (stem_name, stem_path) in &result.stem_files {
            let new_track_name = format!("{} - {}", track_name, stem_name);
            
            // Create new track
            let track_result = e.execute("track.new", &json!({
                "count": 1,
                "format": "stereo",
                "name": new_track_name
            })).map_err(|e| bad("audiosuite.stem_separation", format!("failed to create track: {}", e)))?;
            
            log::info!("Track creation result: {:?}", track_result);

            // Import the stem file into the new track using the track name (not ID)
            log::info!("Importing stem file {} into track {}", stem_path, new_track_name);
            let import_result = e.execute("file.import_audio", &json!({
                "path": stem_path,
                "track": new_track_name,
                "at": clip.start
            }));
            
            log::info!("Import result: {:?}", import_result);
            
            if let Err(e) = import_result {
                log::warn!("Failed to import stem file {}: {}", stem_path, e);
            } else {
                log::info!("Successfully imported stem file {}", stem_path);
                created_tracks.push(new_track_name.clone());
            }
            
            results.push(json!({
                "stem": stem_name,
                "track": new_track_name,
                "file": stem_path
            }));
        }
    }

    log::info!("Stem separation complete: {} tracks created", created_tracks.len());
    Ok(json!({
        "stems": results,
        "model": model_name,
        "tracks_created": created_tracks.len(),
        "note": "Created separate tracks for each stem"
    }))
}
