// Stem separation module for SoundCraft
// Uses Charon Audio for Rust-based stem separation

use charon_audio::{Separator, SeparatorConfig};
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::thread;

const MODEL_URL: &str = "https://huggingface.co/StemSplitio/htdemucs-onnx/resolve/main/htdemucs.onnx";

pub struct SeparationResult {
    pub stem_files: HashMap<String, String>,
    pub model: String,
}

/// Progress callback for stem separation
pub type ProgressCallback = Arc<Mutex<dyn Fn(f32, &str) + Send + 'static>>;

/// Download the HTDemucs model if it doesn't exist
fn download_model(model_path: &str, progress: Option<ProgressCallback>) -> Result<(), String> {
    log::info!("Downloading HTDemucs model from Hugging Face...");
    
    if let Some(ref cb) = progress {
        if let Ok(cb) = cb.lock() {
            cb(0.0, "Downloading model...");
        }
    }
    
    let response = ureq::get(MODEL_URL)
        .call()
        .map_err(|e| format!("Failed to download model: {}", e))?;
    
    let total_size = response
        .header("Content-Length")
        .and_then(|h| h.parse::<usize>().ok())
        .unwrap_or(0);
    
    let mut reader = response.into_reader();
    let mut data = Vec::new();
    if total_size > 0 {
        data.reserve(total_size);
    }
    
    std::io::copy(&mut reader, &mut data)
        .map_err(|e| format!("Failed to read model data: {}", e))?;
    
    // Ensure the directory exists
    if let Some(parent) = Path::new(model_path).parent() {
        fs::create_dir_all(parent)
            .map_err(|e| format!("Failed to create model directory: {}", e))?;
    }
    
    fs::write(model_path, data)
        .map_err(|e| format!("Failed to write model file: {}", e))?;
    
    log::info!("Model downloaded successfully to {}", model_path);
    
    if let Some(ref cb) = progress {
        if let Ok(cb) = cb.lock() {
            cb(1.0, "Model downloaded");
        }
    }
    
    Ok(())
}

pub fn separate_audio(
    input_file: &str,
    output_dir: &str,
    model_path: &str,
    progress: Option<ProgressCallback>,
) -> Result<SeparationResult, String> {
    // Download model if it doesn't exist
    if !Path::new(model_path).exists() {
        download_model(model_path, progress.clone())?;
    }
    
    if let Some(ref cb) = progress {
        if let Ok(cb) = cb.lock() {
            cb(0.1, "Initializing separator...");
        }
    }
    
    // Configure separator with progress and try to use GPU
    let config = SeparatorConfig::htdemucs(model_path)
        .with_progress(true)
        .with_overlap(0.5)
        .with_shifts(1);
    
    let separator = Separator::new(config)
        .map_err(|e| format!("Failed to initialize separator: {}", e))?;
    
    log::info!("Starting stem separation for: {}", input_file);
    
    if let Some(ref cb) = progress {
        if let Ok(cb) = cb.lock() {
            cb(0.2, "Separating audio...");
        }
    }
    
    let stems = separator.separate_file(input_file)
        .map_err(|e| format!("Failed to separate audio: {}", e))?;
    
    if let Some(ref cb) = progress {
        if let Ok(cb) = cb.lock() {
            cb(0.9, "Saving stems...");
        }
    }
    
    log::info!("Saving stems to: {}", output_dir);
    stems.save_all(output_dir)
        .map_err(|e| format!("Failed to save stems: {}", e))?;
    
    let mut stem_files = HashMap::new();
    for stem_name in ["drums", "bass", "other", "vocals"] {
        let stem_path = format!("{}/{}.wav", output_dir, stem_name);
        stem_files.insert(stem_name.to_string(), stem_path);
    }
    
    log::info!("Stem separation complete");
    
    if let Some(ref cb) = progress {
        if let Ok(cb) = cb.lock() {
            cb(1.0, "Complete");
        }
    }
    
    Ok(SeparationResult {
        stem_files,
        model: "HTDemucs 4-stem".to_string(),
    })
}

/// Run stem separation in a background thread
pub fn separate_audio_async(
    input_file: String,
    output_dir: String,
    model_path: String,
    progress: ProgressCallback,
) -> thread::JoinHandle<Result<SeparationResult, String>> {
    thread::spawn(move || {
        separate_audio(&input_file, &output_dir, &model_path, Some(progress))
    })
}

pub fn init_stem_separation(model_path: &str) -> Result<(), String> {
    // Download model if it doesn't exist
    if !Path::new(model_path).exists() {
        download_model(model_path, None)?;
    }
    
    let config = SeparatorConfig::htdemucs(model_path);
    let _separator = Separator::new(config)
        .map_err(|e| format!("Failed to initialize separator: {}", e))?;
    Ok(())
}
