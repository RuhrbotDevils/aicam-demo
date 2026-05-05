// Defines Rust configuration and serialization logic for the media service.
// Author: Thomas Klute

//! Recording session management - directory creation and metadata sidecar.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::PathBuf;
use tracing::info;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStream {
    pub stream_type: String,
    pub codec: String,
    pub file_name: String,
    pub status: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMetadata {
    pub session_id: String,
    pub name: Option<String>,
    pub status: String,
    pub directory: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub duration_s: Option<f64>,
    pub streams: Vec<SessionStream>,
    pub video_width: Option<u32>,
    pub video_height: Option<u32>,
    pub video_fps: Option<u32>,
    pub audio_device: Option<String>,
    pub audio_enabled: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_frame_count: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_file_size: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio_file_size: Option<u64>,
}

pub struct RecordingSession {
    pub session_id: String,
    pub name: Option<String>,
    pub directory: PathBuf,
    pub start_time: DateTime<Utc>,
    pub audio_enabled: bool,
    pub video_width: u32,
    pub video_height: u32,
    pub video_fps: u32,
    pub actual_frame_count: Option<u64>,
    pub video_file_size: Option<u64>,
    pub audio_file_size: Option<u64>,
}

/// Validate a session name: only `[a-zA-Z0-9 _-]` allowed.
pub fn is_valid_session_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == ' ' || c == '_' || c == '-')
}

/// Sanitize a session name: replace spaces with underscores.
pub fn sanitize_session_name(name: &str) -> String {
    name.replace(' ', "_")
}

impl RecordingSession {
    pub fn new(
        base_dir: &str,
        audio_enabled: bool,
        width: u32,
        height: u32,
        fps: u32,
        name: Option<&str>,
    ) -> anyhow::Result<Self> {
        let now = Utc::now();
        let timestamp = now.format("%Y-%m-%dT%H-%M-%S").to_string();
        let session_id = match name {
            Some(n) if !n.is_empty() => format!("{}_{}", timestamp, n),
            _ => format!("{}_{}", timestamp, &uuid_short()),
        };
        let directory = std::env::current_dir()
            .unwrap_or_default()
            .join(base_dir)
            .join(&session_id);
        fs::create_dir_all(&directory)?;
        info!(session_id = %session_id, name = ?name, dir = %directory.display(), "Created recording session");

        Ok(Self {
            session_id,
            name: name.map(String::from),
            directory,
            start_time: now,
            audio_enabled,
            video_width: width,
            video_height: height,
            video_fps: fps,
            actual_frame_count: None,
            video_file_size: None,
            audio_file_size: None,
        })
    }

    pub fn video_path(&self) -> PathBuf {
        self.directory.join("video.h264")
    }

    pub fn audio_path(&self) -> PathBuf {
        self.directory.join("audio.flac")
    }

    pub fn metadata_path(&self) -> PathBuf {
        self.directory.join("session.json")
    }

    pub fn pts_path(&self) -> PathBuf {
        self.directory.join("pts.csv")
    }

    pub fn write_metadata(&self, status: &str) -> anyhow::Result<()> {
        let end_time = Utc::now();
        let duration = (end_time - self.start_time).num_milliseconds() as f64 / 1000.0;

        let mut streams = vec![SessionStream {
            stream_type: "video".into(),
            codec: "h264".into(),
            file_name: "video.h264".into(),
            status: status.into(),
        }];

        if self.audio_enabled {
            streams.push(SessionStream {
                stream_type: "audio".into(),
                codec: "flac".into(),
                file_name: "audio.flac".into(),
                status: status.into(),
            });
        }

        let meta = SessionMetadata {
            session_id: self.session_id.clone(),
            name: self.name.clone(),
            status: status.into(),
            directory: self.directory.display().to_string(),
            start_time: Some(self.start_time.to_rfc3339()),
            end_time: Some(end_time.to_rfc3339()),
            duration_s: Some(duration),
            streams,
            video_width: Some(self.video_width),
            video_height: Some(self.video_height),
            video_fps: Some(self.video_fps),
            audio_device: None,
            audio_enabled: self.audio_enabled,
            actual_frame_count: self.actual_frame_count,
            video_file_size: self.video_file_size,
            audio_file_size: self.audio_file_size,
        };

        let json = serde_json::to_string_pretty(&meta)?;
        fs::write(self.metadata_path(), json)?;
        info!(
            session_id = %self.session_id,
            duration_s = duration,
            status = status,
            "Wrote session metadata"
        );
        Ok(())
    }

    /// Populate file sizes by reading from disk. Call after recording stops.
    pub fn collect_file_sizes(&mut self) {
        self.video_file_size = fs::metadata(self.video_path()).ok().map(|m| m.len());
        if self.audio_enabled {
            self.audio_file_size = fs::metadata(self.audio_path()).ok().map(|m| m.len());
        }
    }

    /// Write PTS timestamps to pts.csv.
    pub fn write_pts_csv(&self, pts_entries: &[(u64, u64)]) -> anyhow::Result<()> {
        let mut content = String::from("frame_index,pts_ns\n");
        for (idx, pts) in pts_entries {
            content.push_str(&format!("{},{}\n", idx, pts));
        }
        fs::write(self.pts_path(), content)?;
        info!(
            session_id = %self.session_id,
            frames = pts_entries.len(),
            "Wrote PTS log"
        );
        Ok(())
    }
}

fn uuid_short() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .subsec_nanos();
    format!("{:08x}", t)
}
