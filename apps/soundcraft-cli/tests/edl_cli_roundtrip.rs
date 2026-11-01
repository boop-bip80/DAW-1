//! Public-CLI, hardware-free CMX3600 audio EDL interchange regression.
//! Only one mono A1 cut track, 24 fps non-drop timecode and frame-aligned edits.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 48_000;
const FRAMES: usize = 24_000;
const FRAME_BOUNDARY: usize = 12_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "soundcraft-cmx3600-cli-{}-{nonce}",
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

fn success(label: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{label} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn command_result(output: &Output, command: &str) -> serde_json::Value {
    let text = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{command}: ");
    let json = text
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("{command} had no result in stdout: {text}"));
    serde_json::from_str(json).unwrap()
}

fn original_audio(path: &Path) {
    let mut audio = AudioBuffer::new(RATE, 1, FRAMES);
    for (i, sample) in audio.channels[0].iter_mut().enumerate() {
        let angle = std::f64::consts::TAU * 400.0 * i as f64 / f64::from(RATE);
        *sample = 0.3 * angle.sin() as f32;
    }
    let options = EncodeOptions {
        format: FileFormat::Wav,
        bit_depth: BitDepth::Int16,
        dither: false,
        bwf: None,
    };
    std::fs::write(path, soundcraft_audio_io::encode(&audio, &options).unwrap()).unwrap();
}

fn read_mix(path: &Path) -> AudioBuffer {
    let wav = std::fs::read(path).unwrap();
    let (info, audio) = soundcraft_audio_io::decode(&wav, Some("wav")).unwrap();
    assert_eq!(info.sample_rate, RATE, "unexpected mix sample rate");
    assert_eq!(audio.frames(), FRAMES, "unexpected bounced duration");
    assert_eq!(audio.num_channels(), 2, "main mix must be stereo");
    assert!(
        audio.channels.iter().flatten().all(|sample| sample.is_finite()),
        "EDL roundtrip produced a non-finite audio sample"
    );
    assert!(
        audio.channels.iter().flatten().any(|sample| sample.abs() > 0.01),
        "EDL roundtrip produced silence"
    );
    audio
}

#[test]
fn edl_export_import_preserves_frame_aligned_audio_and_rejects_drop_frame() {
    let dir = Scratch::new();
    let original = dir.file("Voice.wav");
    let project = dir.file("edit.scraft");
    let edl = dir.file("cut.edl");
    let before = dir.file("before.wav");
    let after = dir.file("after.wav");
    let invalid = dir.file("invalid.edl");
    let bad_session = dir.file("bad-session.scraft");
    let bad_mix = dir.file("bad-bounce.wav");
    original_audio(&original);

    // At 48 kHz and 24 fps each frame is exactly 2,000 samples. Splitting
    // after 12,000 samples produces two six-frame cuts from the same source.
    assert_eq!(RATE as usize / 24 * 6, FRAME_BOUNDARY);
    let import = format!(
        "file.import_audio={}",
        serde_json::json!({"path": original.to_string_lossy()})
    );
    let save = format!(
        "session.save={}",
        serde_json::json!({"path": project.to_string_lossy()})
    );
    let export = format!(
        "file.export_edl={}",
        serde_json::json!({"path": edl.to_string_lossy(), "track": "Voice", "fps": 24})
    );
    let exported = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import])
        .args(["--cmd", r#"edit.select={"tracks":["Voice"]}"#])
        .args(["--cmd", r#"edit.separate={"tracks":["Voice"],"at":12000}"#])
        .args(["--cmd", &save])
        .args(["--cmd", &export])
        .arg("--bounce")
        .arg(&before)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    success("create two cuts and export CMX3600", &exported);
    let details = command_result(&exported, "file.export_edl");
    assert_eq!(details["edits"], 2, "expected two six-frame cut events");
    assert_eq!(details["unique_sources"], 1, "both cuts should reference one media source");
    assert!(project.is_file() && edl.is_file());

    let text = std::fs::read_to_string(&edl).unwrap();
    assert!(text.contains("FCM: NON-DROP FRAME"), "EDL is not non-drop");
    assert_eq!(
        text.lines()
            .filter(|line| line.starts_with("00") && line.contains(" A     C "))
            .count(),
        2,
        "EDL did not write exactly two A1 straight-cut events"
    );

    let import_edl = format!(
        "file.import_edl={}",
        serde_json::json!({"path": edl.to_string_lossy(), "fps": 24})
    );
    let imported = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_edl])
        .arg("--bounce")
        .arg(&after)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    success("import CMX3600 and bounce", &imported);
    let details = command_result(&imported, "file.import_edl");
    assert_eq!(details["audio_tracks"], 1, "EDL should import one audio track");
    assert_eq!(details["audio_clips"], 2, "EDL should import two cuts");
    assert_eq!(details["media_sources"], 1, "media reference should be shared");

    let a = read_mix(&before);
    let b = read_mix(&after);
    for (left, right) in a.channels.iter().zip(&b.channels) {
        for (x, y) in left.iter().zip(right) {
            assert!(
                (x - y).abs() < 0.001,
                "CMX3600 export/import changed a bounced sample"
            );
        }
    }

    // Unsupported drop-frame EDL must fail before rendering or saving outputs.
    let broken = text.replace("FCM: NON-DROP FRAME", "FCM: DROP FRAME");
    assert_ne!(broken, text, "malformed fixture did not alter the EDL");
    std::fs::write(&invalid, broken).unwrap();
    let import_bad = format!(
        "file.import_edl={}",
        serde_json::json!({"path": invalid.to_string_lossy(), "fps": 24})
    );
    let rejected = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_bad])
        .arg("--bounce")
        .arg(&bad_mix)
        .arg("--save")
        .arg(&bad_session)
        .output()
        .unwrap();
    assert!(!rejected.status.success(), "drop-frame EDL was wrongly accepted");
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("CMX3600"),
        "failure did not originate in the EDL validator: {}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!bad_mix.exists() && !bad_session.exists(), "invalid EDL produced output files");
}
