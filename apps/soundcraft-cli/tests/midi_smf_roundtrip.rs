//! Public-CLI, hardware-free Standard MIDI File interoperability regression.
//! A generated type-1 SMF must survive import, native save/reopen, export and
//! independent reimport with note, controller and tempo semantics preserved.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use serde_json::{Value, json};
use soundcraft_engine::{Engine, model::ClipContent};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Scratch(PathBuf);

impl Scratch {
    fn new() -> Self {
        let nonce = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("soundcraft-smf-cli-{}-{nonce}", std::process::id()));
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

fn command_result(output: &Output, command: &str) -> Value {
    let stdout = String::from_utf8_lossy(&output.stdout);
    let prefix = format!("{command}: ");
    let line = stdout.lines().find_map(|line| line.strip_prefix(&prefix)).unwrap_or_else(|| panic!("{command} missing from stdout: {stdout}"));
    serde_json::from_str(line).unwrap()
}

/// An independent, tiny SMF 1.0 type-1 fixture at 480 file ticks per quarter.
/// Import normalizes to SoundCraft's 960 model ticks/quarter. Conductor events:
/// 120 BPM and 3/4 at tick zero, then 100 BPM at file tick 960.
fn midi_fixture() -> Vec<u8> {
    const CONDUCTOR: &[u8] = &[
        0x00, 0xff, 0x51, 0x03, 0x07, 0xa1, 0x20, // 120 BPM
        0x00, 0xff, 0x58, 0x04, 0x03, 0x02, 0x18, 0x08, // 3/4
        0x87, 0x40, 0xff, 0x51, 0x03, 0x09, 0x27, 0xc0, // +960 ticks, 100 BPM
        0x00, 0xff, 0x2f, 0x00, // end
    ];
    const KEYS: &[u8] = &[
        0x00, 0xff, 0x03, 0x04, b'K', b'e', b'y', b's',
        0x00, 0xc0, 0x29, // program 41, channel 1
        0x00, 0xb0, 0x07, 0x5a, // CC7 volume 90
        0x00, 0xe0, 0x00, 0x40, // centered pitch wheel
        0x00, 0x90, 0x3c, 0x64, // C4 on, velocity 100
        0x83, 0x60, 0x80, 0x3c, 0x40, // +480, C4 off, release 64
        0x81, 0x70, 0x90, 0x40, 0x50, // +240, E4 on, velocity 80
        0x83, 0x60, 0x80, 0x40, 0x40, // +480, E4 off
        0x00, 0xff, 0x2f, 0x00,
    ];
    const DRUMS: &[u8] = &[
        0x00, 0xff, 0x03, 0x05, b'D', b'r', b'u', b'm', b's',
        0x00, 0x99, 0x24, 0x6e, // channel 10 kick, velocity 110
        0x81, 0x70, 0x89, 0x24, 0x40, // +240, kick off
        0x00, 0xff, 0x2f, 0x00,
    ];
    let mut smf = b"MThd\x00\x00\x00\x06\x00\x01\x00\x03\x01\xe0".to_vec();
    for track in [CONDUCTOR, KEYS, DRUMS] {
        smf.extend_from_slice(b"MTrk");
        smf.extend_from_slice(&u32::try_from(track.len()).unwrap().to_be_bytes());
        smf.extend_from_slice(track);
    }
    smf
}

#[derive(Debug, PartialEq)]
struct MidiSnapshot {
    tracks: Vec<(String, Value)>,
    tempos: Vec<(i64, i64)>,
    meters: Vec<(i64, u32, u32)>,
}

/// Inspect the actual saved project, not a second parse of the original SMF.
fn snapshot(path: &Path) -> MidiSnapshot {
    let mut engine = Engine::default();
    let missing = soundcraft_engine::io::open_session(&mut engine, path.to_str().unwrap()).unwrap();
    assert!(missing.is_empty(), "MIDI-only project unexpectedly depends on external media: {missing:?}");
    let session = engine.session();
    let tracks = session
        .tracks
        .iter()
        .filter(|track| track.kind.is_midi())
        .map(|track| {
            let clips = track
                .clips()
                .iter()
                .filter_map(|clip| match &clip.content {
                    ClipContent::Midi { sequence } => Some(sequence),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(clips.len(), 1, "each imported SMF track should hold one MIDI clip");
            (track.name.clone(), serde_json::to_value(clips[0]).unwrap())
        })
        .collect();
    MidiSnapshot {
        tracks,
        tempos: session.tempo.tempos().iter().map(|t| (t.tick, (t.bpm * 1000.0).round() as i64)).collect(),
        meters: session.tempo.meters().iter().map(|m| (m.tick, m.numerator, m.denominator)).collect(),
    }
}

fn assert_fixture_semantics(saved: &MidiSnapshot) {
    assert_eq!(saved.tracks.len(), 2, "conductor-only SMF track must not become a playable clip");
    assert_eq!(saved.tracks[0].0, "Keys");
    assert_eq!(saved.tracks[1].0, "Drums");

    let keys = &saved.tracks[0].1;
    assert_eq!(
        keys["notes"],
        json!([
            {"pitch": 60, "velocity": 100, "release_velocity": 64, "channel": 0, "start": 0, "length": 960},
            {"pitch": 64, "velocity": 80, "release_velocity": 64, "channel": 0, "start": 1440, "length": 960}
        ]),
        "Keys pitches, velocities or durations changed during import"
    );
    let controls = keys["ctrls"].as_array().unwrap();
    assert_eq!(controls.len(), 3);
    for event in [
        json!({"tick": 0, "channel": 0, "kind": {"Cc": 7}, "value": 90}),
        json!({"tick": 0, "channel": 0, "kind": "Program", "value": 41}),
        json!({"tick": 0, "channel": 0, "kind": "PitchBend", "value": 0}),
    ] {
        assert!(controls.contains(&event), "lost or changed controller {event}");
    }

    assert_eq!(
        saved.tracks[1].1["notes"],
        json!([{"pitch": 36, "velocity": 110, "release_velocity": 64, "channel": 9, "start": 0, "length": 480}]),
        "channel-10 drum notes were not preserved"
    );
    assert!(saved.tempos.contains(&(0, 120_000)), "missing initial 120 BPM conductor tempo: {:?}", saved.tempos);
    assert!(saved.tempos.contains(&(1920, 100_000)), "missing 100 BPM tempo change at model tick 1920: {:?}", saved.tempos);
    assert!(saved.meters.contains(&(0, 3, 4)), "missing 3/4 conductor meter: {:?}", saved.meters);
}

#[test]
fn public_cli_smf_roundtrip_preserves_notes_controllers_and_tempo_without_devices() {
    let tmp = Scratch::new();
    let original_midi = tmp.file("handmade-type1.mid");
    let saved_project = tmp.file("first.scraft");
    let exported_midi = tmp.file("reexported.mid");
    let received_project = tmp.file("received.scraft");
    std::fs::write(&original_midi, midi_fixture()).unwrap();

    let import = format!("file.import_midi={}", json!({"path": original_midi.to_string_lossy()}));
    let initial = cli().args(["run", "--cmd", "session.new", "--cmd", &import]).arg("--save").arg(&saved_project).output().unwrap();
    checked("public MIDI import and native save", &initial);
    assert_eq!(command_result(&initial, "file.import_midi")["tracks"].as_array().unwrap().len(), 2);
    assert!(saved_project.is_file(), "initial MIDI session did not save");
    let first = snapshot(&saved_project);
    assert_fixture_semantics(&first);

    let export = format!("file.export_midi={}", json!({"path": exported_midi.to_string_lossy()}));
    let exported = cli().arg("run").arg("--in").arg(&saved_project).arg("--cmd").arg(&export).output().unwrap();
    checked("reopen native project and publicly export MIDI", &exported);
    assert!(command_result(&exported, "file.export_midi")["bytes"].as_u64().unwrap() > 50);
    let bytes = std::fs::read(&exported_midi).unwrap();
    assert!(bytes.starts_with(b"MThd"), "export is missing Standard MIDI File header");
    assert_eq!(u16::from_be_bytes([bytes[8], bytes[9]]), 1, "export should be SMF format 1");
    std::fs::remove_file(&original_midi).unwrap();

    let reimport = format!("file.import_midi={}", json!({"path": exported_midi.to_string_lossy()}));
    let received = cli().args(["run", "--cmd", "session.new", "--cmd", &reimport]).arg("--save").arg(&received_project).output().unwrap();
    checked("fresh public MIDI reimport after deleting source file", &received);
    assert_eq!(command_result(&received, "file.import_midi")["tracks"].as_array().unwrap().len(), 2);
    let second = snapshot(&received_project);
    assert_fixture_semantics(&second);
    assert_eq!(second, first, "public CLI MIDI export/reimport changed persisted event semantics");

    // Fail closed: a truncated header cannot modify an already saved project,
    // run a later export command or save any partially parsed MIDI tracks.
    let malformed = tmp.file("truncated.mid");
    let should_not_export = tmp.file("rejected-export.mid");
    std::fs::write(&malformed, b"MThd\x00\x00\x00\x06\x00").unwrap();
    let import_bad = format!("file.import_midi={}", json!({"path": malformed.to_string_lossy()}));
    let export_bad = format!("file.export_midi={}", json!({"path": should_not_export.to_string_lossy()}));
    let before_bad = std::fs::read(&received_project).unwrap();
    let rejected = cli()
        .arg("run")
        .arg("--in")
        .arg(&received_project)
        .args(["--cmd", &import_bad, "--cmd", &export_bad])
        .arg("--save")
        .arg(&received_project)
        .output()
        .unwrap();
    assert!(!rejected.status.success(), "truncated SMF was accepted");
    assert!(!should_not_export.exists(), "a later export ran after failed import");
    assert_eq!(std::fs::read(&received_project).unwrap(), before_bad, "malformed import changed the saved session");
    assert_eq!(snapshot(&received_project), second, "malformed import changed MIDI notes or tempo");
}
