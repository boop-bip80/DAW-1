//! AAC packets from Symphonia's demuxer, decoded with SBR/PS and multichannel support.
//! The scalar-only dependency has no native runtime or unsafe acceleration enabled.

use symphonia::core::{codecs::CodecParameters, errors::Error as SymError, formats::FormatReader, units::TimeBase};

use crate::{AudioBuffer, AudioError, AudioInfo, Result, pcm::check_decode_samples};

fn decoder_error(error: rusty_aac::Error) -> AudioError {
    match error {
        rusty_aac::Error::Unsupported(message) => AudioError::Unsupported(format!("AAC: {message}")),
        other => AudioError::Malformed(format!("AAC: {other}")),
    }
}

/// Container trim counts use its time base, which may be the AAC core's rate.
fn trim_frames(trim: u32, time_base: TimeBase, output_rate: u32) -> Result<usize> {
    if time_base.denom == 0 || time_base.numer == 0 {
        return Err(AudioError::Malformed("AAC has an invalid time base".into()));
    }
    let frames = u128::from(trim) * u128::from(time_base.numer) * u128::from(output_rate) / u128::from(time_base.denom);
    usize::try_from(frames).map_err(|_| AudioError::TooLarge("AAC trim exceeds addressable memory".into()))
}

pub(crate) fn run(
    mut reader: Box<dyn FormatReader>,
    params: &CodecParameters,
    track: u32,
    mut info: AudioInfo,
    count_only: bool,
) -> Result<(AudioInfo, AudioBuffer)> {
    let mut decoder = match params.extra_data.as_deref() {
        Some(config) => rusty_aac::AacDecoder::with_config_bytes(config).map_err(decoder_error)?,
        None => {
            let channels = params.channels.map(|c| c.count()).unwrap_or(usize::from(info.channels));
            // ADTS demuxing supplies the standard AAC configuration's channel count.
            let configuration = match channels {
                1..=6 => channels as u16,
                8 => 7,
                _ => return Err(AudioError::Unsupported("AAC requires a supported channel configuration".into())),
            };
            rusty_aac::AacDecoder::with_config(rusty_aac::AudioSpecificConfig {
                object_type: 2,
                sample_rate: info.sample_rate,
                channels: configuration,
            })
        }
    };
    let time_base = params.time_base.unwrap_or(TimeBase { numer: 1, denom: info.sample_rate });
    let mut planar: Vec<Vec<f32>> = Vec::new();
    let mut frames = 0u64;
    let mut format = None;
    loop {
        let packet = match reader.next_packet() {
            Ok(packet) => packet,
            Err(SymError::IoError(error)) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(error) => return Err(AudioError::Malformed(format!("AAC container: {error}"))),
        };
        if packet.track_id() != track {
            continue;
        }
        let decoded = decoder.decode(packet.buf(), None).map_err(decoder_error)?;
        let channels = usize::from(decoded.channels);
        if decoded.sample_rate == 0 || !(1..=8).contains(&channels) || !decoded.samples.len().is_multiple_of(channels) {
            return Err(AudioError::Malformed("AAC decoder returned an invalid audio layout".into()));
        }
        let current = (decoded.sample_rate, decoded.channels);
        if format.is_some_and(|previous| previous != current) {
            return Err(AudioError::Unsupported("AAC changes sample rate or channel layout within the stream".into()));
        }
        if format.is_none() {
            format = Some(current);
            info.sample_rate = decoded.sample_rate;
            info.channels = decoded.channels;
            if !count_only {
                planar = (0..channels).map(|_| Vec::new()).collect();
            }
        }
        let start = trim_frames(packet.trim_start(), time_base, decoded.sample_rate)?;
        let end_trim = trim_frames(packet.trim_end(), time_base, decoded.sample_rate)?;
        let n = decoded.frames();
        let end = n
            .checked_sub(end_trim)
            .filter(|&end| start <= end)
            .ok_or_else(|| AudioError::Malformed("AAC packet trim exceeds decoded frames".into()))?;
        frames = frames.checked_add((end - start) as u64).ok_or_else(|| AudioError::TooLarge("AAC frame count overflow".into()))?;
        check_decode_samples(frames, decoded.channels, decoded.sample_rate)?;
        if !count_only {
            for dst in &mut planar {
                dst.try_reserve(end - start).map_err(|_| AudioError::TooLarge("not enough memory to decode AAC".into()))?;
            }
            let samples =
                decoded.samples.get(start * channels..end * channels).ok_or_else(|| AudioError::Malformed("AAC trim is out of range".into()))?;
            for frame in samples.chunks_exact(channels) {
                for (dst, &sample) in planar.iter_mut().zip(frame) {
                    dst.push(if sample.is_finite() { sample } else { 0.0 });
                }
            }
        }
    }
    if format.is_none() {
        return Err(AudioError::Malformed("AAC stream contains no audio packets".into()));
    }
    info.frames = frames;
    Ok((info.clone(), AudioBuffer { sample_rate: info.sample_rate, channels: planar }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trims_follow_the_output_rate_and_reject_invalid_time_bases() {
        assert_eq!(trim_frames(512, TimeBase { numer: 1, denom: 24_000 }, 48_000).unwrap(), 1024);
        assert_eq!(trim_frames(1024, TimeBase { numer: 1, denom: 48_000 }, 48_000).unwrap(), 1024);
        assert!(trim_frames(1, TimeBase { numer: 1, denom: 0 }, 48_000).is_err());
    }
}
