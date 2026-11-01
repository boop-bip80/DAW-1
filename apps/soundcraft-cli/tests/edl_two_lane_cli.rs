//! Hardware-free public CLI end-to-end regression for independent CMX3600 A1/A2 mono lanes.
//! Unlike linked-stereo AA, these lanes have separate sources and overlapping record cuts.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 48_000;
const FRAMES: usize = 24_000;
const LEFT_HZ: f64 = 400.0;
const RIGHT_HZ: f64 = 800.0;
const LEFT_LEVEL: f32 = 0.30;
const RIGHT_LEVEL: f32 = 0.18;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let dir = std::env::temp_dir().join(format!("soundcraft-a1-a2-cli-{}-{nonce}", std::process::id()));
        std::fs::create_dir(&dir).unwrap();
        Self(dir)
    }

    fn file(&self, name: &str) -> PathBuf {
        self.0.join(name)
    }

    fn subdir(&self, name: &str) -> PathBuf {
        let path = self.file(name);
        std::fs::create_dir(&path).unwrap();
        path
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

fn result_json(output: &Output, command: &str) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{command}: ");
    let line =
        stdout.lines().find_map(|line| line.strip_prefix(&prefix)).unwrap_or_else(|| panic!("{command} result missing from CLI stdout:\n{stdout}"));
    serde_json::from_str(line).unwrap()
}

fn write_tone(path: &Path, frequency: f64, level: f32) {
    let mut audio = AudioBuffer::new(RATE, 1, FRAMES);
    for (i, sample) in audio.channels[0].iter_mut().enumerate() {
        let radians = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
        *sample = level * radians.sin() as f32;
    }
    let options = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Int16, dither: false, bwf: None };
    std::fs::write(path, soundcraft_audio_io::encode(&audio, &options).unwrap()).unwrap();
}

fn read_mix(path: &Path) -> AudioBuffer {
    let wav = std::fs::read(path).unwrap();
    let (info, audio) = soundcraft_audio_io::decode(&wav, Some("wav")).unwrap();
    assert_eq!(info.sample_rate, RATE, "bounce rate changed");
    assert_eq!(audio.frames(), FRAMES, "bounce duration changed");
    assert_eq!(audio.num_channels(), 2, "main output must remain stereo");
    assert!(audio.channels.iter().flatten().all(|sample| sample.is_finite()), "A1/A2 bounce contains non-finite samples");
    audio
}

/// Both test frequencies fall on exact spectral bins over the 500-ms duration.
fn magnitude(samples: &[f32], frequency: f64) -> f64 {
    let (mut real, mut imag) = (0.0, 0.0);
    for (i, sample) in samples.iter().enumerate() {
        let radians = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
        real += f64::from(*sample) * radians.cos();
        imag += f64::from(*sample) * radians.sin();
    }
    2.0 * real.hypot(imag) / samples.len() as f64
}

fn verify_independent_lanes(audio: &AudioBuffer) {
    let left_400 = magnitude(&audio.channels[0], LEFT_HZ);
    let left_800 = magnitude(&audio.channels[0], RIGHT_HZ);
    let right_400 = magnitude(&audio.channels[1], LEFT_HZ);
    let right_800 = magnitude(&audio.channels[1], RIGHT_HZ);
    assert!(left_400 > 0.18 && left_400 < 0.34, "left source gain unexpected: {left_400}");
    assert!(right_800 > 0.10 && right_800 < 0.21, "right source gain unexpected: {right_800}");
    assert!(
        left_800 < left_400 * 0.01 && right_400 < right_800 * 0.01,
        "mono A1/A2 lanes were swapped, merged or leaked: L400={left_400}, L800={left_800}, R400={right_400}, R800={right_800}"
    );
    let ratio = right_800 / left_400;
    let expected = f64::from(RIGHT_LEVEL / LEFT_LEVEL);
    assert!((ratio - expected).abs() < 0.03, "independent lane gain ratio changed: {ratio} vs {expected}");
}

fn equal_mixes(label: &str, original: &AudioBuffer, other: &AudioBuffer) {
    for (channel, (before, after)) in original.channels.iter().zip(&other.channels).enumerate() {
        let max_difference = before.iter().zip(after).map(|(a, b)| (a - b).abs()).fold(0.0_f32, f32::max);
        assert!(max_difference < 0.001, "{label} changed channel {channel}: maximum sample difference {max_difference}");
    }
}

#[test]
fn independent_a1_a2_interleaved_edl_survives_import_bounce_and_media_handoff() {
    let dir = Scratch::new();
    let source_dir = dir.subdir("sources");
    let original_dir = dir.subdir("original");
    let exchange_dir = dir.subdir("exchange");
    let copied_dir = dir.subdir("imported");
    let left_source = source_dir.join("Left400.wav");
    let right_source = source_dir.join("Right800.wav");
    let original_project = original_dir.join("two-mono.scraft");
    let imported_project = copied_dir.join("from-edl.scraft");
    let edl = exchange_dir.join("interleaved.edl");
    let first_mix = dir.file("original-mix.wav");
    let imported_mix = dir.file("imported-mix.wav");
    let reopened_mix = dir.file("reopened-mix.wav");
    let invalid_edl = exchange_dir.join("mixed-independent-linked.edl");
    let invalid_project = dir.file("invalid.scraft");
    let invalid_mix = dir.file("invalid.wav");
    write_tone(&left_source, LEFT_HZ, LEFT_LEVEL);
    write_tone(&right_source, RIGHT_HZ, RIGHT_LEVEL);

    // Each 24-fps frame has exactly 2,000 samples. Splitting A1 at six
    // frames and A2 at four makes events overlap across (but not within)
    // lanes, so a mono/stereo flattening bug cannot pass this check.
    assert_eq!(RATE as usize / 24, 2_000);
    let import = format!("file.import_audio={}", serde_json::json!({"paths":[left_source.to_string_lossy(), right_source.to_string_lossy()]}));
    let save_original = format!("session.save={}", serde_json::json!({"path":original_project.to_string_lossy()}));
    let export = format!("file.export_edl={}", serde_json::json!({"path":edl.to_string_lossy(), "tracks":["Left400","Right800"], "fps":24}));
    let exported = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import])
        .args(["--cmd", r#"edit.select={"tracks":["Left400"]}"#])
        .args(["--cmd", r#"edit.separate={"tracks":["Left400"],"at":12000}"#])
        .args(["--cmd", r#"edit.select={"tracks":["Right800"]}"#])
        .args(["--cmd", r#"edit.separate={"tracks":["Right800"],"at":8000}"#])
        .args(["--cmd", &save_original])
        .args(["--cmd", &export])
        // EDL cannot encode mixer pan: make the reference stereo bounce
        // only after exporting the unprocessed mono lanes.
        .args(["--cmd", r#"mix.pan={"track":"Left400","pan":-1}"#])
        .args(["--cmd", r#"mix.pan={"track":"Right800","pan":1}"#])
        .arg("--bounce")
        .arg(&first_mix)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    checked("import two mono sources, staggered splits, A1/A2 export", &exported);
    let details = result_json(&exported, "file.export_edl");
    assert_eq!(details["edits"], 4, "each mono lane must contain two cuts");
    assert_eq!(details["unique_sources"], 2, "mono lanes must retain distinct media");
    assert!(original_project.is_file() && edl.is_file(), "project or EDL was not saved");

    let text = std::fs::read_to_string(&edl).unwrap();
    assert!(text.contains("FCM: NON-DROP FRAME"), "expected 24 fps non-drop EDL");
    let events: Vec<Vec<&str>> = text
        .lines()
        .filter(|line| line.split_whitespace().next().is_some_and(|first| first.len() == 3 && first.bytes().all(|byte| byte.is_ascii_digit())))
        .map(|line| line.split_whitespace().collect())
        .collect();
    assert_eq!(events.len(), 4, "expected four cut events");
    assert_eq!(
        events.iter().map(|event| event[2]).collect::<Vec<_>>(),
        ["A1", "A2", "A2", "A1"],
        "staggered overlapping mono lanes were not interleaved in time order"
    );
    assert_eq!(events[2][6], "00:00:00:04", "A2 second cut should start at frame 4");
    assert_eq!(events[3][6], "00:00:00:06", "A1 second cut should start at frame 6");

    let import_edl = format!("file.import_edl={}", serde_json::json!({"path":edl.to_string_lossy(), "fps":24}));
    let imported = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_edl])
        .args(["--cmd", r#"mix.pan={"track":"A1","pan":-1}"#])
        .args(["--cmd", r#"mix.pan={"track":"A2","pan":1}"#])
        .arg("--bounce")
        .arg(&imported_mix)
        .args(["--start", "0", "--end", "0.5"])
        .arg("--save")
        .arg(&imported_project)
        .output()
        .unwrap();
    checked("fresh A1/A2 import, independent pan, bounce and save", &imported);
    let details = result_json(&imported, "file.import_edl");
    assert_eq!(details["audio_tracks"], 2, "A1/A2 must import as two mono tracks");
    assert_eq!(details["audio_clips"], 4, "both lane-specific cuts must survive");
    assert_eq!(details["media_sources"], 2, "media sources must not be coalesced");
    assert!(imported_project.is_file(), "the imported project was not saved");

    let before = read_mix(&first_mix);
    let after = read_mix(&imported_mix);
    verify_independent_lanes(&before);
    verify_independent_lanes(&after);
    equal_mixes("EDL import", &before, &after);

    // A linked-stereo event cannot be spliced into independent A1/A2 lanes.
    // Import must fail before the CLI writes its optional output artifacts.
    let broken = text.replacen(" A2    C ", " AA    C ", 1);
    assert_ne!(broken, text, "malformed fixture did not alter an A2 event");
    std::fs::write(&invalid_edl, broken).unwrap();
    let import_invalid = format!("file.import_edl={}", serde_json::json!({"path":invalid_edl.to_string_lossy(), "fps":24}));
    let rejected = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_invalid])
        .arg("--bounce")
        .arg(&invalid_mix)
        .arg("--save")
        .arg(&invalid_project)
        .output()
        .unwrap();
    assert!(!rejected.status.success(), "mixed independent/linked lanes were accepted");
    assert!(
        String::from_utf8_lossy(&rejected.stderr).contains("ambiguous channel semantics"),
        "mixed-lane rejection reported an unexpected error:\n{}",
        String::from_utf8_lossy(&rejected.stderr)
    );
    assert!(!invalid_project.exists() && !invalid_mix.exists(), "rejected EDL produced a saved session or bounced audio");

    // The imported project must carry its own media. Remove both original
    // source WAVs and the project whose media URLs the EDL used, then reopen.
    std::fs::remove_dir_all(&source_dir).unwrap();
    std::fs::remove_dir_all(&original_dir).unwrap();
    let reopened = cli()
        .arg("run")
        .arg("--in")
        .arg(&imported_project)
        .arg("--bounce")
        .arg(&reopened_mix)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    checked("reopen independent A1/A2 session without original media", &reopened);
    assert!(!String::from_utf8_lossy(&reopened.stderr).contains("warning: missing media"), "reopened session lost its imported mono media");
    let reopened = read_mix(&reopened_mix);
    verify_independent_lanes(&reopened);
    equal_mixes("save/reopen without source project", &after, &reopened);
}
