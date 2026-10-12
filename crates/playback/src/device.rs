//! Audio device enumeration and hardware capability inspection.

use serde::{Deserialize, Serialize};

/// POD audio device capability descriptor, mirroring hardware HAL DeviceInfo.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioDeviceDescriptor {
    /// Human-readable device name or identifier.
    pub name: String,
    /// Maximum simultaneous input channels.
    pub max_input_channels: u16,
    /// Maximum simultaneous output channels.
    pub max_output_channels: u16,
    /// Default native sample rate in Hz.
    pub default_sample_rate: u32,
    /// Supported sample rates (e.g. 44100, 48000, 88200, 96000, 192000).
    pub supported_sample_rates: Vec<u32>,
    /// Minimum buffer size supported (if reported by driver).
    pub min_buffer_size: Option<u32>,
    /// Maximum buffer size supported (if reported by driver).
    pub max_buffer_size: Option<u32>,
    /// True if system default input device.
    pub is_default_input: bool,
    /// True if system default output device.
    pub is_default_output: bool,
}

#[cfg(not(target_arch = "wasm32"))]
pub fn list_output_devices() -> Vec<AudioDeviceDescriptor> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let default_output = host.default_output_device().and_then(|d| d.name().ok());
    let default_input = host.default_input_device().and_then(|d| d.name().ok());
    let mut out = Vec::new();
    if let Ok(devices) = host.output_devices() {
        for d in devices {
            if let Ok(name) = d.name() {
                let is_default_out = default_output.as_ref().is_some_and(|n| n == &name);
                let is_default_in = default_input.as_ref().is_some_and(|n| n == &name);
                out.push(inspect_device(&d, name, is_default_in, is_default_out));
            }
        }
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
pub fn list_input_devices() -> Vec<AudioDeviceDescriptor> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    let default_output = host.default_output_device().and_then(|d| d.name().ok());
    let default_input = host.default_input_device().and_then(|d| d.name().ok());
    let mut out = Vec::new();
    if let Ok(devices) = host.input_devices() {
        for d in devices {
            if let Ok(name) = d.name() {
                let is_default_out = default_output.as_ref().is_some_and(|n| n == &name);
                let is_default_in = default_input.as_ref().is_some_and(|n| n == &name);
                out.push(inspect_device(&d, name, is_default_in, is_default_out));
            }
        }
    }
    out
}

#[cfg(not(target_arch = "wasm32"))]
fn inspect_device(d: &cpal::Device, name: String, is_default_input: bool, is_default_output: bool) -> AudioDeviceDescriptor {
    use cpal::traits::DeviceTrait;
    let mut max_in = 0;
    let mut max_out = 0;
    let mut sample_rates = std::collections::BTreeSet::new();
    let mut min_buf = None;
    let mut max_buf = None;

    if let Ok(configs) = d.supported_output_configs() {
        for c in configs {
            max_out = max_out.max(c.channels());
            let min_sr = c.min_sample_rate().0;
            let max_sr = c.max_sample_rate().0;
            for std_rate in [44100, 48000, 88200, 96000, 176400, 192000] {
                if std_rate >= min_sr && std_rate <= max_sr {
                    sample_rates.insert(std_rate);
                }
            }
            match c.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    min_buf = Some(min_buf.map_or(*min, |b: u32| b.min(*min)));
                    max_buf = Some(max_buf.map_or(*max, |b: u32| b.max(*max)));
                }
                cpal::SupportedBufferSize::Unknown => {}
            }
        }
    }

    if let Ok(configs) = d.supported_input_configs() {
        for c in configs {
            max_in = max_in.max(c.channels());
            let min_sr = c.min_sample_rate().0;
            let max_sr = c.max_sample_rate().0;
            for std_rate in [44100, 48000, 88200, 96000, 176400, 192000] {
                if std_rate >= min_sr && std_rate <= max_sr {
                    sample_rates.insert(std_rate);
                }
            }
            match c.buffer_size() {
                cpal::SupportedBufferSize::Range { min, max } => {
                    min_buf = Some(min_buf.map_or(*min, |b: u32| b.min(*min)));
                    max_buf = Some(max_buf.map_or(*max, |b: u32| b.max(*max)));
                }
                cpal::SupportedBufferSize::Unknown => {}
            }
        }
    }

    let default_sr = d.default_output_config().or_else(|_| d.default_input_config()).map(|c| c.sample_rate().0).unwrap_or(48000);

    AudioDeviceDescriptor {
        name,
        max_input_channels: max_in,
        max_output_channels: max_out,
        default_sample_rate: default_sr,
        supported_sample_rates: sample_rates.into_iter().collect(),
        min_buffer_size: min_buf,
        max_buffer_size: max_buf,
        is_default_input,
        is_default_output,
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn find_output_device(name: Option<&str>) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if let (Some(target), Ok(devices)) = (name, host.output_devices()) {
        for d in devices {
            if d.name().map(|n| n.eq_ignore_ascii_case(target)).unwrap_or(false) {
                return Some(d);
            }
        }
    }
    host.default_output_device()
}

#[cfg(not(target_arch = "wasm32"))]
pub fn find_input_device(name: Option<&str>) -> Option<cpal::Device> {
    use cpal::traits::{DeviceTrait, HostTrait};
    let host = cpal::default_host();
    if let (Some(target), Ok(devices)) = (name, host.input_devices()) {
        for d in devices {
            if d.name().map(|n| n.eq_ignore_ascii_case(target)).unwrap_or(false) {
                return Some(d);
            }
        }
    }
    host.default_input_device()
}

#[cfg(target_arch = "wasm32")]
pub fn list_output_devices() -> Vec<AudioDeviceDescriptor> {
    vec![AudioDeviceDescriptor {
        name: "Web Audio".into(),
        max_input_channels: 0,
        max_output_channels: 2,
        default_sample_rate: 44100,
        supported_sample_rates: vec![44100, 48000],
        min_buffer_size: Some(128),
        max_buffer_size: Some(2048),
        is_default_input: false,
        is_default_output: true,
    }]
}

#[cfg(target_arch = "wasm32")]
pub fn list_input_devices() -> Vec<AudioDeviceDescriptor> {
    Vec::new()
}

#[cfg(target_arch = "wasm32")]
pub fn find_output_device(_name: Option<&str>) -> Option<cpal::Device> {
    use cpal::traits::HostTrait;
    cpal::default_host().default_output_device()
}

#[cfg(target_arch = "wasm32")]
pub fn find_input_device(_name: Option<&str>) -> Option<cpal::Device> {
    use cpal::traits::HostTrait;
    cpal::default_host().default_input_device()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_serialization_roundtrip() {
        let desc = AudioDeviceDescriptor {
            name: "Test Interface".into(),
            max_input_channels: 2,
            max_output_channels: 8,
            default_sample_rate: 48000,
            supported_sample_rates: vec![44100, 48000, 96000],
            min_buffer_size: Some(64),
            max_buffer_size: Some(2048),
            is_default_input: true,
            is_default_output: false,
        };
        let serialized = serde_json::to_string(&desc).unwrap();
        let parsed: AudioDeviceDescriptor = serde_json::from_str(&serialized).unwrap();
        assert_eq!(desc, parsed);
    }

    #[test]
    fn device_listing_does_not_panic() {
        let outs = list_output_devices();
        let ins = list_input_devices();
        // Devices may or may not be present in CI/headless, but must never panic
        assert!(outs.iter().all(|d| !d.name.is_empty()));
        assert!(ins.iter().all(|d| !d.name.is_empty()));
    }
}
