//! Regression for File → Save Session Copy In: media stays with the delivered
//! project even if the original recording and session are no longer available.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 48_000;
const FRAMES: usize = 12_000;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "soundcraft-session-handoff-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir(&path).unwrap();
        Self(path)
    }

    fn path(&self, name: &str) -> PathBuf {
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

fn tone(path: &Path, frequency: f64) {
    let mut audio = AudioBuffer::new(RATE, 1, FRAMES);
    for (i, sample) in audio.channels[0].iter_mut().enumerate() {
        let phase = std::f64::consts::TAU * frequency * i as f64 / f64::from(RATE);
        *sample = 0.18 * phase.sin() as f32;
    }
    let options = EncodeOptions {
        format: FileFormat::Wav,
        bit_depth: BitDepth::Int16,
        dither: false,
        bwf: None,
    };
    std::fs::write(path, soundcraft_audio_io::encode(&audio, &options).unwrap()).unwrap();
}

fn bounce(path: &Path) -> AudioBuffer {
    let bytes = std::fs::read(path).unwrap();
    let (info, audio) = soundcraft_audio_io::decode(&bytes, Some("wav")).unwrap();
    assert_eq!(info.sample_rate, RATE, "unexpected mix sample rate");
    assert_eq!(audio.frames(), FRAMES, "unexpected mix duration");
    assert_eq!(audio.num_channels(), 2, "expected the stereo main bus");
    assert!(
        audio.channels.iter().flatten().all(|sample| sample.is_finite()),
        "mix contains non-finite samples"
    );
    assert!(
        audio.channels.iter().flatten().any(|sample| sample.abs() > 0.01),
        "mix is unexpectedly silent"
    );
    audio
}

fn audio_files(dir: &Path) -> usize {
    std::fs::read_dir(dir.join("Audio Files"))
        .unwrap()
        .flatten()
        .filter(|entry| entry.path().extension().and_then(std::ffi::OsStr::to_str) == Some("wav"))
        .count()
}

#[test]
fn save_session_copy_is_portable_and_refuses_unrequested_overwrite() {
    let tmp = Scratch::new();
    let import_dir = tmp.path("recordings");
    std::fs::create_dir(&import_dir).unwrap();
    let voice = import_dir.join("Voice.wav");
    let music = import_dir.join("Music.wav");
    tone(&voice, 400.0);
    tone(&music, 700.0);

    let original_dir = tmp.path("working-project");
    let project = original_dir.join("working.scraft");
    let handoff_dir = tmp.path("handoff");
    let copy = handoff_dir.join("deliverable.scraft");
    let original_mix = tmp.path("baseline.wav");
    let copied_mix = tmp.path("handoff-mix.wav");

    let import = format!(
        "file.import_audio={}",
        serde_json::json!({"paths": [voice.to_string_lossy(), music.to_string_lossy()]})
    );
    let initial = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import])
        .arg("--bounce")
        .arg(&original_mix)
        .args(["--start", "0", "--end", "0.25"])
        .arg("--save")
        .arg(&project)
        .output()
        .unwrap();
    success("create original session", &initial);
    assert!(project.is_file(), "working session was not saved");
    assert_eq!(audio_files(&original_dir), 2, "working session lost source audio");

    let copy_command = format!(
        "session.save_copy={}",
        serde_json::json!({"path": copy.to_string_lossy()})
    );
    let copy_result = cli()
        .arg("run")
        .arg("--in")
        .arg(&project)
        .arg("--cmd")
        .arg(&copy_command)
        .output()
        .unwrap();
    success("save project copy", &copy_result);
    assert!(copy.is_file(), "session copy was not created");
    assert_eq!(audio_files(&handoff_dir), 2, "session copy omitted or duplicated media");

    // A second Save Copy to an existing destination must refuse replacement
    // without an explicit opt-in, leaving the handed-off project untouched.
    let copy_before = std::fs::read(&copy).unwrap();
    let replace_attempt = cli()
        .arg("run")
        .arg("--in")
        .arg(&project)
        .arg("--cmd")
        .arg(&copy_command)
        .output()
        .unwrap();
    assert!(
        !replace_attempt.status.success(),
        "Save Copy silently overwrote an existing project"
    );
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        copy_before,
        "failed Save Copy modified the existing session"
    );

    // Remove both imported recordings and the entire original project folder.
    // Only the copy in the independently generated handoff directory remains.
    std::fs::remove_dir_all(&import_dir).unwrap();
    std::fs::remove_dir_all(&original_dir).unwrap();
    let reopened = cli()
        .arg("run")
        .arg("--in")
        .arg(&copy)
        .arg("--bounce")
        .arg(&copied_mix)
        .args(["--start", "0", "--end", "0.25"])
        .output()
        .unwrap();
    success("reopen handed-off project", &reopened);
    assert!(
        !String::from_utf8_lossy(&reopened.stderr).contains("warning: missing media"),
        "project copy still depends on the deleted original media"
    );

    let before = bounce(&original_mix);
    let after = bounce(&copied_mix);
    for (original_channel, copied_channel) in before.channels.iter().zip(&after.channels) {
        for (a, b) in original_channel.iter().zip(copied_channel) {
            assert!(
                (a - b).abs() < 0.001,
                "handoff changed the decoded mix samples"
            );
        }
    }
}
