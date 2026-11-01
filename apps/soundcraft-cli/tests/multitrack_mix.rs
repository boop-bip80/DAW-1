//! End-to-end multitrack mix regression: independent input frequencies, hard pan,
//! fader gain, offline bounce and save/reopen without sound hardware.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 48_000;
const FRAMES: usize = 12_000;
const INPUT_AMPLITUDE: f32 = 0.35;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "soundcraft-multitrack-{}-{nonce}",
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

fn checked(step: &str, output: Output) -> Output {
    assert!(
        output.status.success(),
        "{step} failed: {:?}\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn write_tone(path: &Path, frequency: f64) {
    let mut audio = AudioBuffer::new(RATE, 1, FRAMES);
    for (i, sample) in audio.channels[0].iter_mut().enumerate() {
        let phase = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
        *sample = INPUT_AMPLITUDE * phase.sin() as f32;
    }
    let options = EncodeOptions {
        format: FileFormat::Wav,
        bit_depth: BitDepth::Int16,
        dither: false,
        bwf: None,
    };
    let bytes = soundcraft_audio_io::encode(&audio, &options).unwrap();
    std::fs::write(path, bytes).unwrap();
}

fn read_bounce(path: &Path) -> AudioBuffer {
    let bytes = std::fs::read(path).unwrap();
    let (info, audio) = soundcraft_audio_io::decode(&bytes, Some("wav")).unwrap();
    assert_eq!(info.sample_rate, RATE, "unexpected bounce sample rate");
    assert_eq!(audio.frames(), FRAMES, "unexpected bounce length");
    assert_eq!(audio.num_channels(), 2, "main mix must be stereo");
    assert!(
        audio.channels.iter().flatten().all(|s| s.is_finite()),
        "the bounce contains non-finite audio"
    );
    audio
}

/// Sine magnitude at an exact bin in a 250-ms, 48 kHz buffer. Frequencies 400
/// and 800 Hz each complete an integer number of cycles and do not overlap.
fn magnitude(samples: &[f32], frequency: f64) -> f64 {
    let (mut real, mut imag) = (0.0, 0.0);
    for (i, sample) in samples.iter().enumerate() {
        let angle = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
        real += f64::from(*sample) * angle.cos();
        imag += f64::from(*sample) * angle.sin();
    }
    2.0 * real.hypot(imag) / samples.len() as f64
}

#[test]
fn multitrack_pan_fader_and_media_survive_roundtrip() {
    let dir = Scratch::new();
    let left = dir.file("Left400.wav");
    let right = dir.file("Right800.wav");
    let before = dir.file("first-mix.wav");
    let session = dir.file("multitrack.scraft");
    let after = dir.file("reopened-mix.wav");
    write_tone(&left, 400.0);
    write_tone(&right, 800.0);

    let import = format!(
        "file.import_audio={}",
        serde_json::json!({
            "paths": [left.to_string_lossy(), right.to_string_lossy()]
        })
    );
    let first = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import])
        .args(["--cmd", r#"mix.pan={"track":"Left400","pan":-1}"#])
        .args(["--cmd", r#"mix.pan={"track":"Right800","pan":1}"#])
        .args(["--cmd", r#"mix.volume={"track":"Right800","db":-6}"#])
        .arg("--bounce")
        .arg(&before)
        .args(["--start", "0", "--end", "0.25"])
        .arg("--save")
        .arg(&session)
        .output()
        .unwrap();
    checked("two-track import, pan, fader, bounce and save", first);
    assert!(session.is_file(), "session was not saved");
    assert!(
        std::fs::read_dir(dir.file("Audio Files"))
            .unwrap()
            .flatten()
            .filter(|entry| entry.path().extension().and_then(std::ffi::OsStr::to_str) == Some("wav"))
            .count()
            >= 2,
        "save did not persist both independent audio sources"
    );

    let mix_before = read_bounce(&before);
    let left_400 = magnitude(&mix_before.channels[0], 400.0);
    let left_800 = magnitude(&mix_before.channels[0], 800.0);
    let right_400 = magnitude(&mix_before.channels[1], 400.0);
    let right_800 = magnitude(&mix_before.channels[1], 800.0);

    assert!(left_400 > 0.20, "left 400 Hz program is inaudible: {left_400}");
    assert!(right_800 > 0.10, "right 800 Hz program is inaudible: {right_800}");
    assert!(
        left_800 < left_400 * 0.01 && right_400 < right_800 * 0.01,
        "hard pan leaked across channels: left400={left_400}, left800={left_800}, right400={right_400}, right800={right_800}"
    );
    let measured_ratio = right_800 / left_400;
    let expected_ratio = 10.0_f64.powf(-6.0 / 20.0);
    assert!(
        (measured_ratio - expected_ratio).abs() < 0.03,
        "-6 dB fader did not produce the expected gain ratio: observed={measured_ratio}, expected={expected_ratio}"
    );

    // A saved project must carry its own copied media, not depend on import paths.
    std::fs::remove_file(left).unwrap();
    std::fs::remove_file(right).unwrap();
    let reopened = cli()
        .arg("run")
        .arg("--in")
        .arg(&session)
        .arg("--bounce")
        .arg(&after)
        .args(["--start", "0", "--end", "0.25"])
        .output()
        .unwrap();
    let reopened = checked("reopen and bounce", reopened);
    assert!(
        !String::from_utf8_lossy(&reopened.stderr).contains("warning: missing media"),
        "reopened session reports missing saved media"
    );

    let mix_after = read_bounce(&after);
    for (a, b) in mix_before.channels.iter().zip(&mix_after.channels) {
        for (first, second) in a.iter().zip(b) {
            assert!(
                (first - second).abs() < 0.001,
                "mix samples changed after session save/reopen"
            );
        }
    }
}
