//! Public CLI export-stems regression: every saved audio track must render to
//! its own isolated, finite WAV and survive reopening without original media.
//! This exercises actual file.bounce_stems, not the main-mix bounce shortcut.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 48_000;
const FRAMES: usize = 12_000;
const VOICE_HZ: f64 = 400.0;
const PAD_HZ: f64 = 800.0;
const AMPLITUDE: f32 = 0.32;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("soundcraft-stems-cli-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn cli() -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_soundcraft-cli"));
    cmd.env("SOUNDCRAFT_NO_PREFS", "1");
    cmd
}

fn checked(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn command_result(output: &Output, command: &str) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{command}: ");
    let line = stdout.lines().find_map(|line| line.strip_prefix(&prefix)).unwrap_or_else(|| panic!("{command} had no result: {stdout}"));
    serde_json::from_str(line).unwrap()
}

fn write_tone(path: &Path, frequency: f64) {
    let mut audio = AudioBuffer::new(RATE, 1, FRAMES);
    for (index, sample) in audio.channels[0].iter_mut().enumerate() {
        let phase = std::f64::consts::TAU * frequency * index as f64 / f64::from(RATE);
        *sample = AMPLITUDE * phase.sin() as f32;
    }
    let settings = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Int16, dither: false, bwf: None };
    std::fs::write(path, soundcraft_audio_io::encode(&audio, &settings).unwrap()).unwrap();
}

fn read_wav(path: &Path) -> AudioBuffer {
    let (info, audio) = soundcraft_audio_io::decode(&std::fs::read(path).unwrap(), Some("wav")).unwrap();
    assert_eq!(info.sample_rate, RATE, "stem sample rate changed");
    assert_eq!(audio.frames(), FRAMES, "stem duration changed");
    assert_eq!(audio.num_channels(), 2, "stems must preserve the stereo main route");
    assert!(audio.channels.iter().flatten().all(|sample| sample.is_finite()), "non-finite stem samples");
    audio
}

fn magnitude(samples: &[f32], frequency: f64) -> f64 {
    let (mut real, mut imaginary) = (0.0, 0.0);
    for (index, sample) in samples.iter().enumerate() {
        let phase = std::f64::consts::TAU * frequency * index as f64 / f64::from(RATE);
        real += f64::from(*sample) * phase.cos();
        imaginary += f64::from(*sample) * phase.sin();
    }
    2.0 * real.hypot(imaginary) / samples.len() as f64
}

fn check_isolation(stem: &AudioBuffer, expected_channel: usize, expected_hz: f64, other_hz: f64) {
    let wanted = magnitude(&stem.channels[expected_channel], expected_hz);
    let other_frequency = magnitude(&stem.channels[expected_channel], other_hz);
    let wrong_channel = magnitude(&stem.channels[1 - expected_channel], expected_hz);
    assert!(wanted > 0.19 && wanted < 0.35, "stem has wrong gain for {expected_hz} Hz: {wanted}");
    assert!(other_frequency < wanted * 0.01, "stem leaked {other_hz} Hz from another track: {other_frequency}");
    assert!(wrong_channel < wanted * 0.01, "hard-panned stem leaked to opposite channel: {wrong_channel}");
}

struct Stems {
    voice: AudioBuffer,
    pad: AudioBuffer,
}

fn read_stems(output: &Output, directory: &Path) -> Stems {
    let result = command_result(output, "file.bounce_stems");
    let files = result["files"].as_array().expect("stem command should return a files array");
    assert_eq!(files.len(), 2, "two active audio tracks must produce two stem files");
    let mut voice = None;
    let mut pad = None;
    for file in files {
        let path = PathBuf::from(file.as_str().expect("stem path must be a string"));
        assert!(path.starts_with(directory), "stem escaped its requested output folder: {}", path.display());
        assert!(path.is_file(), "stem path returned by CLI does not exist: {}", path.display());
        let name = path.file_name().unwrap().to_string_lossy();
        if name.contains("Voice400") {
            assert!(voice.replace(read_wav(&path)).is_none(), "duplicate Voice400 stem");
        } else if name.contains("Pad800") {
            assert!(pad.replace(read_wav(&path)).is_none(), "duplicate Pad800 stem");
        } else {
            panic!("unexpected stem filename: {name}");
        }
    }
    assert_eq!(std::fs::read_dir(directory).unwrap().count(), 2, "stem export left unexpected files");
    let voice = voice.expect("Voice400 stem missing");
    let pad = pad.expect("Pad800 stem missing");
    check_isolation(&voice, 0, VOICE_HZ, PAD_HZ);
    check_isolation(&pad, 1, PAD_HZ, VOICE_HZ);
    Stems { voice, pad }
}

fn assert_same_stem(first: &AudioBuffer, second: &AudioBuffer, label: &str) {
    assert_eq!(first.num_channels(), second.num_channels());
    assert_eq!(first.frames(), second.frames());
    for (channel, (before, after)) in first.channels.iter().zip(&second.channels).enumerate() {
        let max_delta = before.iter().zip(after).map(|(a, b)| (a - b).abs()).fold(0.0_f32, f32::max);
        assert!(max_delta < 0.001, "reopened {label} stem channel {channel} changed: max delta {max_delta}");
    }
}

#[test]
fn public_cli_stem_bounce_remains_isolated_after_save_reopen_and_source_removal() {
    let tmp = Scratch::new();
    let recordings = tmp.file("source-recordings");
    std::fs::create_dir(&recordings).unwrap();
    let voice_source = recordings.join("Voice400.wav");
    let pad_source = recordings.join("Pad800.wav");
    let project = tmp.file("stems-project.scraft");
    let original_stems = tmp.file("stems-before");
    let reopened_stems = tmp.file("stems-after");
    let rejected_stems = tmp.file("stems-rejected");
    write_tone(&voice_source, VOICE_HZ);
    write_tone(&pad_source, PAD_HZ);

    // Saving first makes the project own both imported audio files. The original
    // recording directory can then disappear without breaking stem export.
    let import = format!(
        "file.import_audio={}",
        serde_json::json!({"paths": [voice_source.to_string_lossy(), pad_source.to_string_lossy()]})
    );
    let saved = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import])
        .args(["--cmd", r#"mix.pan={"track":"Voice400","pan":-1}"#])
        .args(["--cmd", r#"mix.pan={"track":"Pad800","pan":1}"#])
        .arg("--save")
        .arg(&project)
        .output()
        .unwrap();
    checked("import two recordings, hard-pan and save", &saved);
    assert!(project.is_file(), "stem project did not save");

    let bounce_before = format!(
        "file.bounce_stems={}",
        serde_json::json!({
            "dir": original_stems.to_string_lossy(), "format": "wav", "bit_depth": 16,
            "dither": false, "start": {"seconds": 0.0}, "end": {"seconds": 0.25}
        })
    );
    let first = cli().arg("run").arg("--in").arg(&project).arg("--cmd").arg(&bounce_before).output().unwrap();
    checked("public two-track stem export", &first);
    let first_stems = read_stems(&first, &original_stems);

    // This is a deliberate user error: an unsupported fold-down target must
    // reject before creating output files or modifying the saved session.
    let session_before_failure = std::fs::read(&project).unwrap();
    let invalid = format!(
        "file.bounce_stems={}",
        serde_json::json!({"dir": rejected_stems.to_string_lossy(), "fold_down": "quad"})
    );
    let rejected =
        cli().arg("run").arg("--in").arg(&project).arg("--cmd").arg(&invalid).arg("--save").arg(&project).output().unwrap();
    assert!(!rejected.status.success(), "unsupported stem fold_down was accepted");
    assert!(!rejected_stems.exists(), "failed stem export created an output directory");
    assert_eq!(std::fs::read(&project).unwrap(), session_before_failure, "failed stem export changed saved project");

    std::fs::remove_dir_all(&recordings).unwrap();
    assert!(!voice_source.exists() && !pad_source.exists());
    let bounce_after = format!(
        "file.bounce_stems={}",
        serde_json::json!({
            "dir": reopened_stems.to_string_lossy(), "format": "wav", "bit_depth": 16,
            "dither": false, "start": {"seconds": 0.0}, "end": {"seconds": 0.25}
        })
    );
    let again = cli().arg("run").arg("--in").arg(&project).arg("--cmd").arg(&bounce_after).output().unwrap();
    checked("reopen saved session and export stems without source recordings", &again);
    assert!(
        !String::from_utf8_lossy(&again.stderr).contains("warning: missing media"),
        "reopened project still needs removed source recordings:\n{}",
        String::from_utf8_lossy(&again.stderr)
    );
    let second_stems = read_stems(&again, &reopened_stems);
    assert_same_stem(&first_stems.voice, &second_stems.voice, "Voice400");
    assert_same_stem(&first_stems.pad, &second_stems.pad, "Pad800");
}
