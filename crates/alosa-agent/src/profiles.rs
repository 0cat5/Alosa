//! Capability presets for each device type. On real hardware these will come
//! from a board config file; for now they let one binary act as any device.

use alosa_protocol::v1::{self, AudioProtocol, DisplayKind, LedKind, PowerKind, SensorKind};
use clap::ValueEnum;

#[derive(Clone, Copy, Debug, ValueEnum)]
pub enum Profile {
    /// Line-in/line-out audio bridge (like a Sonos Connect).
    Connect,
    /// Smart speaker with microphones and an LED ring.
    Speaker,
    /// Touchscreen smart display.
    Display,
    /// Battery e-ink clock (calendar + weather).
    EinkClock,
}

impl Profile {
    pub fn model(self) -> &'static str {
        match self {
            Profile::Connect => "alosa-connect",
            Profile::Speaker => "alosa-speaker",
            Profile::Display => "alosa-display",
            Profile::EinkClock => "alosa-eink-clock",
        }
    }

    /// Name shown in Music Assistant's device info.
    pub fn product_name(self) -> &'static str {
        match self {
            Profile::Connect => "Alosa Connect",
            Profile::Speaker => "Alosa Speaker",
            Profile::Display => "Alosa Display",
            Profile::EinkClock => "Alosa E-ink Clock",
        }
    }

    pub fn has_audio(self) -> bool {
        self.capabilities(Vec::new()).audio_out.is_some()
    }

    /// The device's hardware. `outputs` are the audio outputs found on this
    /// machine; profiles without audio ignore them.
    pub fn capabilities(self, outputs: Vec<v1::AudioOutput>) -> v1::Capabilities {
        // AirPlay and Spotify Connect reach the device through Music
        // Assistant's plugins, so Sendspin is the only protocol it speaks.
        let audio = |channels| v1::AudioOut {
            channels,
            protocols: vec![AudioProtocol::Sendspin as i32],
            outputs: outputs.clone(),
            engine: String::new(), // filled in by the agent
        };
        let mains = Some(v1::Power {
            kind: PowerKind::Mains as i32,
            deep_sleep: false,
        });

        match self {
            Profile::Connect => v1::Capabilities {
                audio_out: Some(audio(2)),
                power: mains,
                leds: Some(v1::Leds {
                    kind: LedKind::Single as i32,
                    count: 1,
                }),
                buttons: 1,
                ..Default::default()
            },
            Profile::Speaker => v1::Capabilities {
                audio_out: Some(audio(1)),
                microphone: Some(v1::Microphone {
                    echo_cancellation: true,
                    on_device_wake_word: true,
                }),
                power: mains,
                leds: Some(v1::Leds {
                    kind: LedKind::Ring as i32,
                    count: 12,
                }),
                buttons: 4,
                ..Default::default()
            },
            Profile::Display => v1::Capabilities {
                audio_out: Some(audio(2)),
                microphone: Some(v1::Microphone {
                    echo_cancellation: true,
                    on_device_wake_word: true,
                }),
                display: Some(v1::Display {
                    kind: DisplayKind::Lcd as i32,
                    width: 1280,
                    height: 800,
                    touch: true,
                    colors: 0,
                }),
                power: mains,
                sensors: vec![SensorKind::Illuminance as i32, SensorKind::Presence as i32],
                ..Default::default()
            },
            Profile::EinkClock => v1::Capabilities {
                display: Some(v1::Display {
                    kind: DisplayKind::Eink as i32,
                    width: 800,
                    height: 480,
                    touch: false,
                    colors: 3,
                }),
                power: Some(v1::Power {
                    kind: PowerKind::Battery as i32,
                    deep_sleep: true,
                }),
                sensors: vec![SensorKind::Temperature as i32, SensorKind::Humidity as i32],
                buttons: 2,
                ..Default::default()
            },
        }
    }
}
