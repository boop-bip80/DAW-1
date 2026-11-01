//! Hardware-free public-CLI CMX3600 AA stereo interchange regression.
//! Only one linked stereo audio lane, straight cuts and 24 fps whole-frame edits.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 48_000;
const FRAMES: usize = 24_000;
const FRAME_BOUNDARY: usize = 12_000;
const LEFT_HZ: f64 = 400.0;
const RIGHT_HZ: f64 = 800.0;
const LEFT_LEVEL: f32 = 0.30;
const RIGHT_LEVEL: f32 = 0.15;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("soundcraft-stereo-aa-cli-{}-{nonce}", std::process::id()));
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
    let text = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{command}: ");
    let result = text.lines().find_map(|line| line.strip_prefix(&prefix)).unwrap_or_else(|| panic!("{command} had no result in stdout: {text}"));
    serde_json::from_str(result).unwrap()
}

fn write_distinct_stereo(path: &Path) {
    let mut audio = AudioBuffer::new(RATE, 2, FRAMES);
    for (channel, frequency, level) in [(0, LEFT_HZ, LEFT_LEVEL), (1, RIGHT_HZ, RIGHT_LEVEL)] {
        for (i, sample) in audio.channels[channel].iter_mut().enumerate() {
            let radians = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
            *sample = level * radians.sin() as f32;
        }
    }
    let options = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Int16, dither: false, bwf: None };
    std::fs::write(path, soundcraft_audio_io::encode(&audio, &options).unwrap()).unwrap();
}

fn read_mix(path: &Path) -> AudioBuffer {
    let bytes = std::fs::read(path).unwrap();
    let (info, audio) = soundcraft_audio_io::decode(&bytes, Some("wav")).unwrap();
    assert_eq!(info.sample_rate, RATE, "unexpected bounce sample rate");
    assert_eq!(audio.frames(), FRAMES, "unexpected bounced duration");
    assert_eq!(audio.num_channels(), 2, "linked AA cut should bounce to stereo");
    assert!(audio.channels.iter().flatten().all(|sample| sample.is_finite()), "stereo EDL roundtrip produced a non-finite audio sample");
    audio
}

/// Both frequencies complete an integer number of cycles in this 500-ms buffer.
fn magnitude(samples: &[f32], frequency: f64) -> f64 {
    let (mut real, mut imag) = (0.0, 0.0);
    for (i, sample) in samples.iter().enumerate() {
        let radians = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
        real += f64::from(*sample) * radians.cos();
        imag += f64::from(*sample) * radians.sin();
    }
    2.0 * real.hypot(imag) / samples.len() as f64
}

fn check_channel_isolation_and_gain(audio: &AudioBuffer) {
    let left_400 = magnitude(&audio.channels[0], LEFT_HZ);
    let left_800 = magnitude(&audio.channels[0], RIGHT_HZ);
    let right_400 = magnitude(&audio.channels[1], LEFT_HZ);
    let right_800 = magnitude(&audio.channels[1], RIGHT_HZ);

    // Broad bounds allow PCM16 rounding and any ordinary stereo pan law but
    // reject missing channels, severe attenuation and duplicated full-scale audio.
    assert!(left_400 > 0.18 && left_400 < 0.34, "left 400-Hz level was altered: {left_400}");
    assert!(right_800 > 0.09 && right_800 < 0.17, "right 800-Hz level was altered: {right_800}");
    assert!(
        left_800 < left_400 * 0.01 && right_400 < right_800 * 0.01,
        "AA import crossed or mixed channels: L400={left_400}, L800={left_800}, R400={right_400}, R800={right_800}"
    );
    let expected_ratio = f64::from(RIGHT_LEVEL / LEFT_LEVEL);
    let observed_ratio = right_800 / left_400;
    assert!(
        (observed_ratio - expected_ratio).abs() < 0.03,
        "linked-stereo channel gain ratio changed: measured={observed_ratio}, expected={expected_ratio}"
    );
}

#[test]
fn stereo_aa_edl_roundtrip_preserves_channels_and_rejects_mixed_a_aa() {
    let dir = Scratch::new();
    let source = dir.file("Stereo.wav");
    let project = dir.file("stereo-edit.scraft");
    let edl = dir.file("stereo.edl");
    let before = dir.file("before.wav");
    let after = dir.file("after.wav");
    let mixed_edl = dir.file("mixed.edl");
    let bad_project = dir.file("mixed-rejected.scraft");
    let bad_bounce = dir.file("mixed-rejected.wav");
    write_distinct_stereo(&source);

    // 48,000 / 24 = 2,000 samples per EDL frame. The middle cut is six frames
    // from time zero and yields exactly two consecutive six-frame AA events.
    assert_eq!(RATE as usize / 24 * 6, FRAME_BOUNDARY);
    let import = format!("file.import_audio={}", serde_json::json!({"path": source.to_string_lossy()}));
    let save = format!("session.save={}", serde_json::json!({"path": project.to_string_lossy()}));
    let export = format!("file.export_edl={}", serde_json::json!({"path": edl.to_string_lossy(), "track": "Stereo", "fps": 24}));
    let exported = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import])
        .args(["--cmd", r#"edit.select={"tracks":["Stereo"]}"#])
        .args(["--cmd", r#"edit.separate={"tracks":["Stereo"],"at":12000}"#])
        .args(["--cmd", &save])
        .args(["--cmd", &export])
        .arg("--bounce")
        .arg(&before)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    checked("import stereo, split, save and export AA EDL", &exported);
    assert_eq!(command_result(&exported, "file.export_edl")["edits"], 2);
    assert_eq!(command_result(&exported, "file.export_edl")["unique_sources"], 1);
    assert!(project.is_file() && edl.is_file(), "EDL or backing session was not saved");

    let text = std::fs::read_to_string(&edl).unwrap();
    assert!(text.contains("FCM: NON-DROP FRAME"), "expected non-drop EDL");
    assert_eq!(
        text.lines().filter(|line| line.starts_with("00") && line.contains(" AA    C ")).count(),
        2,
        "EDL must have two linked stereo AA cuts (not separate mono events)"
    );

    let import_edl = format!("file.import_edl={}", serde_json::json!({"path": edl.to_string_lossy(), "fps": 24}));
    let imported = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_edl])
        .arg("--bounce")
        .arg(&after)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    checked("fresh-session AA EDL import and stereo bounce", &imported);
    let details = command_result(&imported, "file.import_edl");
    assert_eq!(details["audio_tracks"], 1, "AA must import as one linked stereo track");
    assert_eq!(details["audio_clips"], 2, "both frame-exact cuts must survive");
    assert_eq!(details["media_sources"], 1, "both cuts should reuse the same stereo media");

    let original_mix = read_mix(&before);
    let reimported_mix = read_mix(&after);
    check_channel_isolation_and_gain(&original_mix);
    check_channel_isolation_and_gain(&reimported_mix);
    for (channel, (original, reimported)) in original_mix.channels.iter().zip(&reimported_mix.channels).enumerate() {
        let max_delta = original.iter().zip(reimported).map(|(left, right)| (left - right).abs()).fold(0.0_f32, f32::max);
        assert!(max_delta < 0.001, "AA roundtrip changed output channel {channel}: max sample delta {max_delta}");
    }

    // One mono event followed by a stereo AA event is not a linked-stereo EDL.
    // Its import must stop before either optional CLI output is created.
    let mixed = text.replacen(" AA    C ", " A     C ", 1);
    assert_ne!(mixed, text, "failed to mutate the first AA event");
    assert_eq!(mixed.matches(" AA    C ").count(), 1);
    assert_eq!(mixed.matches(" A     C ").count(), 1);
    std::fs::write(&mixed_edl, mixed).unwrap();
    let import_mixed = format!("file.import_edl={}", serde_json::json!({"path": mixed_edl.to_string_lossy(), "fps": 24}));
    let rejected = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_mixed])
        .arg("--bounce")
        .arg(&bad_bounce)
        .arg("--save")
        .arg(&bad_project)
        .output()
        .unwrap();
    assert!(!rejected.status.success(), "mixed mono/stereo EDL was wrongly accepted");
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("mixed A and AA"),
        "failure did not report mixed channel events:\n{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!bad_bounce.exists() && !bad_project.exists(), "a rejected EDL left a bounce or session on disk");
}
