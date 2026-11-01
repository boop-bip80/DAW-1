//! Public-CLI regression: a portable OTIOZ audio bundle must survive deletion of
//! the exporting session, the original recording, and the archive itself.
//! No audio device, external fixture or OpenTimelineIO Python runtime is needed.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use soundcraft_audio_io::{AudioBuffer, BitDepth, EncodeOptions, FileFormat};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const RATE: u32 = 48_000;
const FRAMES: usize = 24_000;
const LEFT_HZ: f64 = 400.0;
const RIGHT_HZ: f64 = 900.0;
const LEFT_LEVEL: f32 = 0.28;
const RIGHT_LEVEL: f32 = 0.12;

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("soundcraft-otioz-cli-{}-{nonce}", std::process::id()));
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

fn checked(step: &str, output: &Output) {
    assert!(
        output.status.success(),
        "{step} failed ({:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

fn command_result(output: &Output, command: &str) -> serde_json::Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{command}: ");
    let line = stdout
        .lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("no {command} result in stdout: {stdout}"));
    serde_json::from_str(line).unwrap()
}

fn write_stereo(path: &Path) {
    let mut audio = AudioBuffer::new(RATE, 2, FRAMES);
    for (channel, hz, level) in [(0, LEFT_HZ, LEFT_LEVEL), (1, RIGHT_HZ, RIGHT_LEVEL)] {
        for (index, sample) in audio.channels[channel].iter_mut().enumerate() {
            let phase = std::f64::consts::TAU * hz * index as f64 / f64::from(RATE);
            *sample = level * phase.sin() as f32;
        }
    }
    let options = EncodeOptions { format: FileFormat::Wav, bit_depth: BitDepth::Int16, dither: false, bwf: None };
    std::fs::write(path, soundcraft_audio_io::encode(&audio, &options).unwrap()).unwrap();
}

fn read_mix(path: &Path) -> AudioBuffer {
    let (info, audio) = soundcraft_audio_io::decode(&std::fs::read(path).unwrap(), Some("wav")).unwrap();
    assert_eq!(info.sample_rate, RATE, "bounce changed sample rate");
    assert_eq!(audio.frames(), FRAMES, "bounce changed duration");
    assert_eq!(audio.num_channels(), 2, "bounce lost the stereo main bus");
    assert!(audio.channels.iter().flatten().all(|sample| sample.is_finite()), "bounce contains NaN/infinity");
    audio
}

fn magnitude(samples: &[f32], frequency: f64) -> f64 {
    let (mut re, mut im) = (0.0, 0.0);
    for (index, sample) in samples.iter().enumerate() {
        let phase = std::f64::consts::TAU * frequency * index as f64 / f64::from(RATE);
        re += f64::from(*sample) * phase.cos();
        im += f64::from(*sample) * phase.sin();
    }
    2.0 * re.hypot(im) / samples.len() as f64
}

fn check_channel_isolation(audio: &AudioBuffer) {
    let left_expected = magnitude(&audio.channels[0], LEFT_HZ);
    let left_wrong = magnitude(&audio.channels[0], RIGHT_HZ);
    let right_wrong = magnitude(&audio.channels[1], LEFT_HZ);
    let right_expected = magnitude(&audio.channels[1], RIGHT_HZ);
    assert!(
        (0.15..0.33).contains(&left_expected) && (0.07..0.16).contains(&right_expected),
        "stereo import changed audible gains: left={left_expected}, right={right_expected}"
    );
    assert!(
        left_wrong < 0.01 * left_expected && right_wrong < 0.01 * right_expected,
        "stereo channels leaked or swapped: L400={left_expected}, L900={left_wrong}, R400={right_wrong}, R900={right_expected}"
    );
    let ratio = right_expected / left_expected;
    let expected = f64::from(RIGHT_LEVEL / LEFT_LEVEL);
    assert!((ratio - expected).abs() < 0.03, "relative stereo gain changed: actual={ratio}, expected={expected}");
}

fn assert_same_audio(original: &AudioBuffer, other: &AudioBuffer, label: &str) {
    assert_eq!(original.num_channels(), other.num_channels());
    assert_eq!(original.frames(), other.frames());
    for (channel, (a, b)) in original.channels.iter().zip(&other.channels).enumerate() {
        let max_delta = a.iter().zip(b).map(|(a, b)| (a - b).abs()).fold(0.0_f32, f32::max);
        assert!(max_delta < 0.001, "{label} channel {channel} changed by {max_delta}");
    }
}

fn staged_media_folders(project: &Path) -> Vec<PathBuf> {
    let audio_files = project.join("Audio Files");
    let Ok(entries) = std::fs::read_dir(audio_files) else { return Vec::new() };
    entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("OTIOZ-import-")))
        .collect()
}

#[test]
fn portable_otioz_cli_import_survives_archive_and_exporter_removal() {
    let tmp = Scratch::new();
    let recordings = tmp.file("recordings");
    let original_dir = tmp.file("original");
    let recipient_dir = tmp.file("recipient");
    std::fs::create_dir(&recordings).unwrap();
    let source = recordings.join("Stereo.wav");
    let project = original_dir.join("original.scraft");
    let recipient = recipient_dir.join("received.scraft");
    let archive = tmp.file("portable.otioz");
    let baseline = tmp.file("baseline.wav");
    let imported_bounce = tmp.file("imported.wav");
    let reopened_bounce = tmp.file("reopened.wav");
    write_stereo(&source);

    // The two cut clips retain independent trims and share one embedded stereo
    // source. CLI argv carries JSON as one argument, so spaces/Windows paths work.
    let import_wav = format!("file.import_audio={}", serde_json::json!({"path": source.to_string_lossy()}));
    // Imported source buffers start out unsaved. Persist the original session
    // and its Audio Files before running the exporter, which requires a real
    // on-disk source rather than an unsaved in-memory recording.
    let original = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_wav])
        .args(["--cmd", r#"edit.select={"tracks":["Stereo"]}"#])
        .args(["--cmd", r#"edit.separate={"tracks":["Stereo"],"at":12000}"#])
        .arg("--save")
        .arg(&project)
        .output()
        .unwrap();
    checked("create, split and save original", &original);
    assert!(project.is_file(), "exporter project was not saved");

    let export_bundle = format!("file.export_otioz={}", serde_json::json!({"path": archive.to_string_lossy()}));
    let exported = cli()
        .arg("run")
        .arg("--in")
        .arg(&project)
        .arg("--cmd")
        .arg(&export_bundle)
        .arg("--bounce")
        .arg(&baseline)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    checked("export OTIOZ from saved source and bounce", &exported);
    assert_eq!(command_result(&exported, "file.export_otioz")["audio_tracks"], 1);
    assert_eq!(command_result(&exported, "file.export_otioz")["audio_clips"], 2);
    assert_eq!(command_result(&exported, "file.export_otioz")["media_sources"], 1);
    assert!(archive.is_file() && std::fs::metadata(&archive).unwrap().len() > 512, "missing or empty exported bundle");
    let expected = read_mix(&baseline);
    check_channel_isolation(&expected);

    // The importer requires an already saved recipient session, not merely a
    // --save option that CLI processes after all --cmd entries.
    let import_bundle = format!("file.import_otioz={}", serde_json::json!({"path": archive.to_string_lossy()}));
    let unsaved_project = tmp.file("unsaved-should-not-exist.scraft");
    let unsaved_bounce = tmp.file("unsaved-should-not-bounce.wav");
    let unsaved = cli()
        .args(["run", "--cmd", "session.new", "--cmd", &import_bundle])
        .arg("--bounce")
        .arg(&unsaved_bounce)
        .arg("--save")
        .arg(&unsaved_project)
        .output()
        .unwrap();
    assert!(!unsaved.status.success(), "OTIOZ import into unsaved session was accepted");
    assert!(
        String::from_utf8_lossy(&unsaved.stderr).contains("save the session"),
        "unsaved-session rejection lacked a useful reason: {}",
        String::from_utf8_lossy(&unsaved.stderr)
    );
    assert!(!unsaved_bounce.exists() && !unsaved_project.exists(), "failed unsaved import left session or bounce output");

    let fresh = cli().args(["run", "--cmd", "session.new"]).arg("--save").arg(&recipient).output().unwrap();
    checked("create saved recipient", &fresh);
    assert!(recipient.is_file(), "recipient session was not saved");

    // Malformed ZIP import must fail before optional CLI bounce/save outputs.
    // The existing recipient bytes must remain identical.
    let malformed = tmp.file("malformed.otioz");
    let malformed_bounce = tmp.file("malformed-should-not-bounce.wav");
    std::fs::write(&malformed, b"not a zip archive").unwrap();
    let before_failed_import = std::fs::read(&recipient).unwrap();
    let rejected_cmd = format!("file.import_otioz={}", serde_json::json!({"path": malformed.to_string_lossy()}));
    let rejected = cli()
        .arg("run")
        .arg("--in")
        .arg(&recipient)
        .arg("--cmd")
        .arg(&rejected_cmd)
        .arg("--bounce")
        .arg(&malformed_bounce)
        .arg("--save")
        .arg(&recipient)
        .output()
        .unwrap();
    assert!(!rejected.status.success(), "corrupt OTIOZ was accepted");
    assert!(!malformed_bounce.exists(), "failed import wrote a bounce");
    assert_eq!(std::fs::read(&recipient).unwrap(), before_failed_import, "failed import modified the saved project");
    assert!(staged_media_folders(&recipient_dir).is_empty(), "corrupt import left staged media");

    // Damage a stored WAV payload while leaving ZIP headers intact. CRC failure
    // must discard the partially written private file and leave the saved
    // recipient untouched, not merely reject a bad ZIP header.
    let broken_crc = tmp.file("broken-media-crc.otioz");
    let broken_bounce = tmp.file("broken-media-should-not-bounce.wav");
    let mut zip_bytes = std::fs::read(&archive).unwrap();
    let riff = zip_bytes.windows(4).position(|bytes| bytes == b"RIFF").expect("exported WAV must appear in stored ZIP");
    zip_bytes[riff + 64] ^= 0x40;
    std::fs::write(&broken_crc, zip_bytes).unwrap();
    let broken_cmd = format!("file.import_otioz={}", serde_json::json!({"path": broken_crc.to_string_lossy()}));
    let broken = cli()
        .arg("run")
        .arg("--in")
        .arg(&recipient)
        .arg("--cmd")
        .arg(&broken_cmd)
        .arg("--bounce")
        .arg(&broken_bounce)
        .arg("--save")
        .arg(&recipient)
        .output()
        .unwrap();
    assert!(!broken.status.success(), "CRC-tampered audio media was accepted");
    assert!(!broken_bounce.exists(), "CRC-failing import wrote a bounce");
    assert_eq!(std::fs::read(&recipient).unwrap(), before_failed_import, "CRC-failing import mutated the saved project");
    assert!(staged_media_folders(&recipient_dir).is_empty(), "CRC-failing import left private staged media");

    // Public import command, bounce and save. Each result count is an oracle:
    // two trims on a single stereo audio track and one deduplicated media source.
    let received = cli()
        .arg("run")
        .arg("--in")
        .arg(&recipient)
        .arg("--cmd")
        .arg(&import_bundle)
        .arg("--bounce")
        .arg(&imported_bounce)
        .args(["--start", "0", "--end", "0.5"])
        .arg("--save")
        .arg(&recipient)
        .output()
        .unwrap();
    checked("import OTIOZ into saved recipient", &received);
    let counts = command_result(&received, "file.import_otioz");
    assert_eq!(counts["audio_tracks"], 1);
    assert_eq!(counts["audio_clips"], 2);
    assert_eq!(counts["media_sources"], 1);
    assert_eq!(counts["extracted_media_files"], 1);
    let folders = staged_media_folders(&recipient_dir);
    assert_eq!(folders.len(), 1, "expected one private imported-media directory");
    let files = std::fs::read_dir(folders[0].join("media")).unwrap().map(|entry| entry.unwrap().path()).collect::<Vec<_>>();
    assert_eq!(files.len(), 1, "deduplicated source was not extracted exactly once");
    assert!(
        files[0].file_name().and_then(|name| name.to_str()).is_some_and(|name| name.starts_with("source-"))
            && files[0].extension().and_then(|ext| ext.to_str()) == Some("wav"),
        "staged media did not have a generated safe name"
    );
    let imported = read_mix(&imported_bounce);
    check_channel_isolation(&imported);
    assert_same_audio(&expected, &imported, "first bundle import");

    // Deliberately remove every exporting dependency, leaving only the recipient
    // session's private extracted media, then reopen in an independent CLI process.
    std::fs::remove_dir_all(&recordings).unwrap();
    std::fs::remove_dir_all(&original_dir).unwrap();
    std::fs::remove_file(&archive).unwrap();
    assert!(!source.exists() && !project.exists() && !archive.exists());
    let reopened = cli()
        .arg("run")
        .arg("--in")
        .arg(&recipient)
        .arg("--bounce")
        .arg(&reopened_bounce)
        .args(["--start", "0", "--end", "0.5"])
        .output()
        .unwrap();
    checked("reopen and rebounce without archive or original media", &reopened);
    assert!(
        !String::from_utf8_lossy(&reopened.stderr).contains("warning: missing media"),
        "reopened recipient still depends on removed exporter data:\n{}",
        String::from_utf8_lossy(&reopened.stderr)
    );
    let after = read_mix(&reopened_bounce);
    check_channel_isolation(&after);
    assert_same_audio(&expected, &after, "reopened bundle import");
}
