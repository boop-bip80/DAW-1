//! Hardware-free end-to-end regression for import → mix bounce → save → reopen.
//! The signal is generated in the test; no bundled recording or audio device is needed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const SAMPLE_RATE: u32 = 48_000;
const FRAMES: usize = 12_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "soundcraft-headless-{}-{nonce}",
            std::process::id()
        ));
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
    let mut command = Command::new(env!("CARGO_BIN_EXE_soundcraft-cli"));
    command.env("SOUNDCRAFT_NO_PREFS", "1");
    command
}

fn assert_success(step: &str, result: &Output) {
    assert!(
        result.status.success(),
        "{step} failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        result.status.code(),
        String::from_utf8_lossy(&result.stdout),
        String::from_utf8_lossy(&result.stderr)
    );
}

fn write_original_tone(path: &Path) {
    let mut audio = AudioBuffer::new(SAMPLE_RATE, 1, FRAMES);
    for (i, sample) in audio.channels[0].iter_mut().enumerate() {
        let phase = (i as f32) * 440.0 * std::f32::consts::TAU / SAMPLE_RATE as f32;
        *sample = 0.4 * phase.sin();
    }
    let options = EncodeOptions {
        format: FileFormat::Wav,
        bit_depth: BitDepth::Int16,
        dither: false,
        bwf: None,
    };
    let wav = soundcraft_audio_io::encode(&audio, &options).unwrap();
    std::fs::write(path, wav).unwrap();
}

fn read_bounce(path: &Path) -> AudioBuffer {
    let bytes = std::fs::read(path).unwrap();
    let (info, audio) = soundcraft_audio_io::decode(&bytes, Some("wav")).unwrap();
    assert_eq!(info.sample_rate, SAMPLE_RATE, "bounce sample rate changed");
    assert_eq!(audio.frames(), FRAMES, "bounce duration changed");
    assert!(!audio.channels.is_empty(), "bounce has no channels");
    assert!(
        audio.channels.iter().flatten().all(|sample| sample.is_finite()),
        "bounce contains a NaN or infinity"
    );
    assert!(
        audio.channels.iter().flatten().any(|sample| sample.abs() > 0.01),
        "bounce is unexpectedly silent"
    );
    audio
}

#[test]
fn imported_audio_survives_save_reopen_and_a_second_bounce() {
    let dir = Scratch::new();
    let source = dir.file("original.wav");
    let session = dir.file("roundtrip.scraft");
    let before = dir.file("before.wav");
    let after = dir.file("after.wav");
    write_original_tone(&source);

    // CLI arguments are transported as real argv entries, with the path safely
    // JSON-escaped. This crosses command dispatch, import, mixing, bounce and save.
    let import = format!(
        "file.import_audio={}",
        serde_json::json!({"path": source.to_string_lossy()})
    );
    let result = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import])
        .arg("--bounce")
        .arg(&before)
        .args(["--start", "0", "--end", "0.25"])
        .arg("--save")
        .arg(&session)
        .output()
        .unwrap();
    assert_success("import/bounce/save", &result);
    assert!(session.is_file(), "save did not write a session file");
    assert!(
        std::fs::metadata(&session).unwrap().len() > 100,
        "session file is unexpectedly empty"
    );

    let audio_folder = dir.file("Audio Files");
    assert!(
        std::fs::read_dir(&audio_folder)
            .unwrap()
            .flatten()
            .any(|entry| entry.path().extension().and_then(std::ffi::OsStr::to_str) == Some("wav")),
        "save did not persist imported media in Audio Files"
    );

    // A saved session must be independent of the original import location.
    std::fs::remove_file(&source).unwrap();
    let result = cli()
        .arg("run")
        .arg("--in")
        .arg(&session)
        .arg("--bounce")
        .arg(&after)
        .args(["--start", "0", "--end", "0.25"])
        .output()
        .unwrap();
    assert_success("reopen/rebounce", &result);
    assert!(
        !String::from_utf8_lossy(&result.stderr).contains("warning: missing media"),
        "saved session still references the deleted original input"
    );

    let a = read_bounce(&before);
    let b = read_bounce(&after);
    assert_eq!(a.num_channels(), b.num_channels());
    for (first, second) in a.channels.iter().zip(&b.channels) {
        for (x, y) in first.iter().zip(second) {
            assert!(
                (x - y).abs() < 0.001,
                "reopened session changed the mix by more than 0.001"
            );
        }
    }
}
