use soundcraft_audio_io::{decode, probe};

#[test]
fn aac_extensions_preserve_duration_rate_and_channels() {
    for (name, bytes, rate, channels) in [
        ("HE v1", include_bytes!("fixtures/he-v1.m4a").as_slice(), 32_000, 2),
        ("HE v2", include_bytes!("fixtures/he-v2.m4a").as_slice(), 48_000, 2),
        ("5.1", include_bytes!("fixtures/aac-51.m4a").as_slice(), 48_000, 6),
        ("LC stereo", include_bytes!("fixtures/lc-stereo.m4a").as_slice(), 48_000, 2),
    ] {
        let (info, audio) = decode(bytes, None).unwrap_or_else(|err| panic!("{name}: {err}"));
        assert_eq!(info.channels, channels, "{name}");
        assert_eq!(info.sample_rate, rate, "{name}");
        assert_eq!(audio.sample_rate, rate, "{name}");
        assert_eq!(audio.frames(), rate as usize, "{name}: one second, including gapless trimming");
        assert_eq!(info.frames, audio.frames() as u64);
        assert_eq!(probe(bytes, None).unwrap(), info, "{name}: probe and decode disagree");
        assert!(audio.channels.iter().flatten().all(|s| s.is_finite()));
    }
}

#[test]
fn adts_without_a_container_config_still_decodes() {
    let (info, audio) = decode(include_bytes!("fixtures/lc-stereo.aac"), None).unwrap();
    assert_eq!((info.sample_rate, info.channels), (48_000, 2));
    assert!(audio.frames() >= 48_000);
    assert_eq!(probe(include_bytes!("fixtures/lc-stereo.aac"), None).unwrap(), info);
}

#[test]
fn truncated_and_mutated_aac_profiles_never_escape_as_panics() {
    for bytes in [include_bytes!("fixtures/he-v2.m4a").as_slice(), include_bytes!("fixtures/aac-51.m4a").as_slice()] {
        for end in (0..bytes.len()).step_by(37) {
            let _ = probe(&bytes[..end], None);
            let _ = decode(&bytes[..end], None);
        }
        for at in (0..bytes.len()).step_by(29) {
            let mut damaged = bytes.to_vec();
            damaged[at] ^= 0xff;
            let _ = decode(&damaged, None);
        }
    }
}
