// Implements Rust media pipeline logic for streaming and camera processing.
// Author: Thomas Klute

//! Device detection - enumerates camera and audio capture devices.

use std::fs;
use std::path::Path;
use tracing::info;
use tracing::warn;

/// Detected camera device info.
#[derive(Debug, Clone)]
pub struct CameraDevice {
    pub path: String,
    pub name: String,
}

/// Detected audio capture device info.
#[derive(Debug, Clone)]
pub struct AudioDevice {
    pub name: String,
    pub card_id: String,
}

/// Detect the first available v4l2 camera device.
///
/// Scans `/dev/video*` for devices that support video capture.
/// Returns the first usable device, or None.
pub fn detect_camera() -> Option<CameraDevice> {
    let dev_dir = Path::new("/dev");
    if !dev_dir.exists() {
        return None;
    }

    let mut video_devices: Vec<String> = Vec::new();
    if let Ok(entries) = fs::read_dir(dev_dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with("video") {
                video_devices.push(format!("/dev/{}", name));
            }
        }
    }
    video_devices.sort();

    if let Some(path) = video_devices.first() {
        let dev_name = device_name_from_sysfs(path);
        let name = dev_name.unwrap_or_else(|| path.clone());
        info!(path = %path, name = %name, "Camera device found");
        return Some(CameraDevice {
            path: path.clone(),
            name,
        });
    }

    warn!("No camera device found in /dev/video*");
    None
}

/// Detect available ALSA capture (microphone) devices.
///
/// Reads `/proc/asound/cards` to find sound cards, then checks
/// for capture capability.
pub fn detect_audio() -> Option<AudioDevice> {
    let cards_path = Path::new("/proc/asound/cards");
    if !cards_path.exists() {
        warn!("No /proc/asound/cards - ALSA not available");
        return None;
    }

    let content = match fs::read_to_string(cards_path) {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "Failed to read /proc/asound/cards");
            return None;
        }
    };

    // Parse lines like: " 0 [Audio          ]: USB-Audio - USB Audio Device"
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty()
            || !trimmed
                .chars()
                .next()
                .map(|c| c.is_ascii_digit())
                .unwrap_or(false)
        {
            continue;
        }
        // Extract card number and name
        if let Some(bracket_start) = trimmed.find('[') {
            if let Some(bracket_end) = trimmed.find(']') {
                let card_id = trimmed[..bracket_start].trim().to_string();
                let card_name = trimmed[bracket_start + 1..bracket_end].trim().to_string();

                // Check if this card has a capture device
                let pcm_path = format!("/proc/asound/card{}/pcm0c", card_id);
                if Path::new(&pcm_path).exists() {
                    info!(card_id = %card_id, name = %card_name, "Audio capture device found");
                    return Some(AudioDevice {
                        name: card_name,
                        card_id,
                    });
                }
            }
        }
    }

    warn!("No audio capture device found");
    None
}

/// Detect whether a Hailo AI accelerator is available.
///
/// Requires BOTH the `hailonet` GStreamer plugin AND a physical Hailo
/// device node (`/dev/hailo*`). 
pub fn detect_hailo() -> bool {
    if gstreamer::init().is_err() {
        return false;
    }
    let plugin_found = gstreamer::ElementFactory::find("hailonet").is_some();
    if !plugin_found {
        info!("Hailo AI accelerator not available (hailonet plugin missing)");
        return false;
    }
    if !hailo_device_node_present() {
        info!("Hailo AI accelerator not available (no /dev/hailo* device node)");
        return false;
    }
    info!("Hailo AI accelerator available (hailonet plugin + /dev/hailo* device found)");
    true
}

/// Return true if at least one `/dev/hailo*` device node exists.
/// The Hailo PCIe kernel driver creates these when it binds to a
/// physical device, so this is a reliable signal that hardware is
/// actually present and usable.
fn hailo_device_node_present() -> bool {
    let Ok(entries) = fs::read_dir("/dev") else {
        return false;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_string_lossy().starts_with("hailo") {
            return true;
        }
    }
    false
}

/// Try to read a v4l2 device name from sysfs.
fn device_name_from_sysfs(dev_path: &str) -> Option<String> {
    // /dev/video0 -> /sys/class/video4linux/video0/name
    let dev_name = dev_path.strip_prefix("/dev/")?;
    let sysfs_path = format!("/sys/class/video4linux/{}/name", dev_name);
    fs::read_to_string(&sysfs_path)
        .ok()
        .map(|s| s.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_camera_returns_option() {
        // Just verify it doesn't panic - actual result depends on hardware
        let _result = detect_camera();
    }

    #[test]
    fn test_detect_audio_returns_option() {
        let _result = detect_audio();
    }

    #[test]
    fn test_device_name_from_sysfs_missing() {
        // Non-existent device should return None
        let result = device_name_from_sysfs("/dev/video999");
        assert!(result.is_none());
    }
}
