use soundcraft_audio_io::probe;

#[test]
fn aiff_accepts_full_unsigned_channel_count() {
    let mut comm = Vec::new();
    comm.extend_from_slice(&32_768u16.to_be_bytes());
    comm.extend_from_slice(&0u32.to_be_bytes());
    comm.extend_from_slice(&16u16.to_be_bytes());
    // 44,100 Hz as an 80-bit extended float.
    comm.extend_from_slice(&[0x40, 0x0e, 0xac, 0x44, 0, 0, 0, 0, 0, 0]);

    let mut file = b"FORM".to_vec();
    file.extend_from_slice(&0u32.to_be_bytes());
    file.extend_from_slice(b"AIFF");
    file.extend_from_slice(b"COMM");
    file.extend_from_slice(&(comm.len() as u32).to_be_bytes());
    file.extend_from_slice(&comm);
    file.extend_from_slice(b"SSND");
    file.extend_from_slice(&8u32.to_be_bytes());
    file.extend_from_slice(&[0; 8]);
    let form_size = (file.len() - 8) as u32;
    file[4..8].copy_from_slice(&form_size.to_be_bytes());

    let info = probe(&file, Some("aiff")).expect("valid AIFF");
    assert_eq!(info.channels, 32_768);
}
