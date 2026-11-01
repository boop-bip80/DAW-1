//! A take being recorded is never replaced or quit away from.
use serde_json::json;
use soundcraft_engine::{Engine, EngineError, TransportRequest, TransportStatus};
use soundcraft_model::{ChannelFormat, TrackKind};
use soundcraft_time::Range;

#[test]
fn nothing_replaces_or_quits_a_session_while_it_records() {
    let mut e = Engine::default();
    e.transport.recording = true;
    for (id, params) in [
        ("session.new", json!({})),
        ("session.open", json!({"path": "Song.scraft"})),
        ("session.close", json!({})),
        ("session.revert", json!({})),
        ("app.quit", json!({})),
    ] {
        assert!(matches!(e.execute(id, &params), Err(EngineError::Disabled(_, why)) if why == "stop recording first"), "{id}");
    }
    assert!(e.execute("track.new", &json!({})).is_ok(), "editing goes on");
    e.transport.recording = false;
    assert!(e.execute("session.close", &json!({})).is_ok());
}

/// These commands previously sought/restarted playback while the WAV/MIDI input writer
/// continued in device time. Reject before mutating even the edit cursor or queue.
#[test]
fn active_recording_rejects_relocation_commands_atomically() {
    let mut e = Engine::default();
    let track = e.session_mut().add_track(TrackKind::Audio, ChannelFormat::Mono, None);
    e.session_mut().track_mut(track).unwrap().mixer.record_arm = true;
    e.session_mut().edit.selection = Range::point(48_000);
    e.session_mut().edit.playhead = 48_000;
    e.mark_clean();
    e.transport = TransportStatus { position: 72_000, playing: true, recording: true };
    let before_edit = e.session().edit.clone();
    let before_transport = e.transport;
    let before_queue = e.transport_requests.clone();

    for (id, params) in [
        ("transport.locate", json!({"at": 96_000})),
        ("transport.rtz", json!({})),
        ("transport.go_to_end", json!({})),
        ("transport.rewind", json!({"seconds": 1})),
        ("transport.fast_forward", json!({"seconds": 1})),
        ("transport.play", json!({"from": 96_000})),
        ("transport.play_selection", json!({})),
        ("edit.play_timeline", json!({})),
        ("markers.recall", json!({"number": 1})),
        ("markers.next", json!({})),
        ("markers.previous", json!({})),
    ] {
        assert!(matches!(e.execute(id, &params), Err(EngineError::Disabled(_, why)) if why == "stop recording first"), "{id}");
        assert_eq!(e.session().edit.selection, before_edit.selection, "{id} changed the selection");
        assert_eq!(e.session().edit.playhead, before_edit.playhead, "{id} changed the edit playhead");
        assert_eq!(e.transport, before_transport, "{id} changed running transport");
        assert_eq!(e.transport_requests, before_queue, "{id} queued a relocation");
        assert!(e.session().track(track).unwrap().mixer.record_arm, "{id} disarmed the track");
        assert!(!e.is_dirty() && !e.can_undo(), "{id} created document history");
    }
    // Non-relocating status reads and explicit Stop/Record-stop remain available.
    assert!(e.execute("transport.status", &json!({})).is_ok());
    e.execute("transport.stop", &json!({})).unwrap();
    e.execute("transport.record", &json!({})).unwrap();
    assert_eq!(e.transport_requests, vec![TransportRequest::Stop, TransportRequest::Record]);
}

#[test]
fn plain_tab_is_blocked_while_recording_but_selection_extension_remains_allowed() {
    let mut e = Engine::default();
    e.session_mut().edit.selection = Range::point(48_000);
    e.mark_clean();
    e.transport = TransportStatus { position: 72_000, playing: true, recording: true };
    let before_selection = e.session().edit.selection;
    let before_playhead = e.session().edit.playhead;
    assert!(matches!(e.execute("edit.tab", &json!({})), Err(EngineError::Disabled(_, why)) if why == "stop recording first"));
    assert_eq!(e.session().edit.selection, before_selection);
    assert_eq!(e.session().edit.playhead, before_playhead);
    assert!(e.transport_requests.is_empty());
    assert!(e.execute("edit.tab", &json!({"extend": true})).is_ok(), "extend does not relocate playback");
    assert!(e.transport_requests.is_empty());
    assert_eq!(e.transport.position, 72_000);
    assert!(!e.is_dirty() && !e.can_undo());
}
