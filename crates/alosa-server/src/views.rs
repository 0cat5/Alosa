//! JSON-friendly views of device capabilities and status for the Home
//! Assistant API.
//!
//! The protobuf types use integer enums (good on the wire, awkward in JSON),
//! so we convert them to readable strings like `"sendspin"` or `"eink"`.

use alosa_protocol::v1;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_out: Option<AudioOut>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub microphone: Option<Microphone>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display: Option<Display>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub power: Option<Power>,
    #[serde(default)]
    pub sensors: Vec<String>,
    #[serde(default)]
    pub buttons: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub leds: Option<Leds>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioOut {
    pub channels: u32,
    pub protocols: Vec<String>,
    #[serde(default)]
    pub outputs: Vec<AudioOutput>,
    /// The player software and version, e.g. "sendspin-cli 0.3.0".
    #[serde(default)]
    pub engine: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AudioOutput {
    pub id: String,
    pub description: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Microphone {
    pub echo_cancellation: bool,
    pub on_device_wake_word: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Display {
    pub kind: String,
    pub width: u32,
    pub height: u32,
    pub touch: bool,
    pub colors: u32,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Power {
    pub kind: String,
    pub deep_sleep: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Leds {
    pub kind: String,
    pub count: u32,
}

/// Turns a generated enum name like `AUDIO_PROTOCOL_SNAPCAST` into `snapcast`.
fn short_name(full: &str, prefix: &str) -> String {
    full.trim_start_matches(prefix).to_ascii_lowercase()
}

/// Converts a raw protobuf enum value to its short name, or "unknown" if the
/// device sent a value this server doesn't know (e.g. newer firmware).
macro_rules! enum_name {
    ($ty:ty, $value:expr, $prefix:literal) => {
        <$ty>::try_from($value)
            .map(|v| short_name(v.as_str_name(), $prefix))
            .unwrap_or_else(|_| "unknown".to_string())
    };
}

impl From<v1::Capabilities> for Capabilities {
    fn from(c: v1::Capabilities) -> Self {
        Self {
            audio_out: c.audio_out.map(|a| AudioOut {
                channels: a.channels,
                protocols: a
                    .protocols
                    .into_iter()
                    .map(|p| enum_name!(v1::AudioProtocol, p, "AUDIO_PROTOCOL_"))
                    .collect(),
                outputs: a
                    .outputs
                    .into_iter()
                    .map(|o| AudioOutput {
                        id: o.id,
                        description: o.description,
                    })
                    .collect(),
                engine: a.engine,
            }),
            microphone: c.microphone.map(|m| Microphone {
                echo_cancellation: m.echo_cancellation,
                on_device_wake_word: m.on_device_wake_word,
            }),
            display: c.display.map(|d| Display {
                kind: enum_name!(v1::DisplayKind, d.kind, "DISPLAY_KIND_"),
                width: d.width,
                height: d.height,
                touch: d.touch,
                colors: d.colors,
            }),
            power: c.power.map(|p| Power {
                kind: enum_name!(v1::PowerKind, p.kind, "POWER_KIND_"),
                deep_sleep: p.deep_sleep,
            }),
            sensors: c
                .sensors
                .into_iter()
                .map(|s| enum_name!(v1::SensorKind, s, "SENSOR_KIND_"))
                .collect(),
            buttons: c.buttons,
            leds: c.leds.map(|l| Leds {
                kind: enum_name!(v1::LedKind, l.kind, "LED_KIND_"),
                count: l.count,
            }),
        }
    }
}

/// What a device is doing right now. Not persisted; cleared when it disconnects.
#[derive(Clone, Debug, Default, Serialize)]
pub struct Status {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub audio: Option<AudioStatus>,
}

#[derive(Clone, Debug, Serialize)]
pub struct AudioStatus {
    /// "stopped", "running" or "failed".
    pub engine: String,
    pub error: String,
    /// The Sendspin server (normally Music Assistant) connected to the device;
    /// empty if none is.
    pub server_name: String,
    /// "idle" or "playing".
    pub playback: String,
}

impl From<v1::Status> for Status {
    fn from(s: v1::Status) -> Self {
        Self {
            audio: s.audio.map(|a| AudioStatus {
                engine: enum_name!(v1::AudioEngineState, a.engine, "AUDIO_ENGINE_STATE_"),
                error: a.error,
                server_name: a.server_name,
                playback: enum_name!(v1::PlaybackState, a.playback, "PLAYBACK_STATE_"),
            }),
        }
    }
}
