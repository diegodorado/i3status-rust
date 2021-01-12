extern crate jack;
extern crate jack_sys;

use dbus;
use dbus::ffidisp::Connection;

use crossbeam_channel::Sender;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::blocks::{Block, ConfigBlock};
use crate::config::{Config};
use crate::errors::*;
use crate::scheduler::Task;
use crate::widget::{I3BarWidget, State};
use crate::widgets::text::TextWidget;

use serde_derive::Deserialize;
use uuid::Uuid;

trait SoundDevice {
    fn volume(&self) -> u32;
    fn muted(&self) -> bool;
    fn jack_running(&self) -> bool;
    fn jack_capturing(&self) -> bool;
    fn jack_rolling(&self) -> bool;

    fn get_info(&mut self) -> Result<()>;
    fn monitor(&mut self, id: String, tx_update_request: Sender<Task>) -> Result<()>;

}

struct JackSoundDevice {
    name: String,
    volume: u32,
    muted: bool,
    jack_running: bool,
    jack_capturing: bool,
    jack_rolling: bool,
}

impl JackSoundDevice {
    fn new(name: String) -> Result<Self> {
        let mut sd = JackSoundDevice {
            name,
            volume: 0,
            muted: false,
            jack_running: false,
            jack_capturing: false,
            jack_rolling: false,
        };
        sd.get_info()?;

        Ok(sd)
    }
}

impl SoundDevice for JackSoundDevice {
    fn volume(&self) -> u32 {
        self.volume
    }

    fn jack_capturing(&self) -> bool {
        self.jack_capturing
    }

    fn jack_rolling(&self) -> bool {
        self.jack_rolling
    }

    fn jack_running(&self) -> bool {
        self.jack_running
    }

    fn muted(&self) -> bool {
        self.muted
    }

    fn get_info(&mut self) -> Result<()> {
        // Create client
        self.jack_capturing = false;
        self.jack_running = false;
        self.jack_rolling = false;
        let c_res = jack::Client::new("rusty_client", jack::ClientOptions::NO_START_SERVER);
        match c_res {
            Ok((client, _status)) => {
                let mut pos = jack_sys::Struct__jack_position {..Default::default()};
                self.jack_rolling = match unsafe {jack_sys::jack_transport_query(client.raw(),&mut pos)} {
                    jack_sys::JackTransportRolling =>true,
                    _ =>false,
                };
                self.jack_running = true;
                if let Some(_port) = client.port_by_name("jack_capture:input1"){
                    self.jack_capturing = true;
                }
            },
            _ =>{},
        };
        let output = Command::new("amixer")
            .args(&["get", &self.name])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_owned())
            .block_error("sound", "could not run amixer to get sound info")?;

        let last_line = &output
            .lines()
            .last()
            .block_error("sound", "could not get sound info")?;

        let last = last_line
            .split_whitespace()
            .filter(|x| x.starts_with('[') && !x.contains("dB"))
            .map(|s| s.trim_matches(FILTER))
            .collect::<Vec<&str>>();

        self.volume = last
            .get(0)
            .block_error("sound", "could not get volume")?
            .parse::<u32>()
            .block_error("sound", "could not parse volume to u32")?;

        self.muted = last.get(1).map(|muted| *muted == "off").unwrap_or(false);

        Ok(())
    }

    
    fn monitor(&mut self, id: String, tx_update_request: Sender<Task>) -> Result<()> {
        
        let id0 = id.clone();
        let txur0 = tx_update_request.clone();
        thread::spawn(move || {
            // Line-buffer to reduce noise.
            let mut monitor = Command::new("stdbuf")
                .args(&["-oL", "alsactl", "monitor"])
                .stdout(Stdio::piped())
                .spawn()
                .expect("Failed to start alsactl monitor")
                .stdout
                .expect("Failed to pipe alsactl monitor output");

            let mut buffer = [0; 1024]; // Should be more than enough.
            loop {
                // Block until we get some output. Doesn't really matter what
                // the output actually is -- these are events -- we just update
                // the sound information if *something* happens.
                if monitor.read(&mut buffer).is_ok() {
                    txur0
                        .send(Task {
                            id: id0.clone(),
                            update_time: Instant::now(),
                        })
                        .unwrap();
                }
                // Don't update too often. Wait 1/4 second, fast enough for
                // volume button mashing but slow enough to skip event spam.
                thread::sleep(Duration::new(0, 250_000_000))
            }
        });

        /*
        let id1 = id.clone();
        let txur1 = tx_update_request.clone();
        thread::spawn(move || {
            // Line-buffer to reduce noise.
            let mut monitor = Command::new("stdbuf")
                .args(&["-oL", "pactl", "subscribe"])
                .stdout(Stdio::piped())
                .spawn()
                .expect("Failed to start pactl monitor")
                .stdout
                .expect("Failed to pipe pactl monitor output");

            let mut buffer = [0; 1024]; // Should be more than enough.
            loop {
                // Block until we get some output. Doesn't really matter what
                // the output actually is -- these are events -- we just update
                // the sound information if *something* happens.
                if monitor.read(&mut buffer).is_ok() {
                    txur1
                        .send(Task {
                            id: id1.clone(),
                            update_time: Instant::now(),
                        })
                        .unwrap();
                }
                // Don't update too often. Wait 1/4 second, fast enough for
                // volume button mashing but slow enough to skip event spam.
                thread::sleep(Duration::new(0, 250_000_000))
            }
        });

        */

        let id2 = id.clone();
        let txur2 = tx_update_request.clone();
        thread::spawn(move || {
            // First open up a connection to the session bus.
            let c = Connection::new_session().unwrap();

            // match server started and stopped events
            c.add_match("interface='org.jackaudio.JackControl',member='ServerStarted'").unwrap();
            c.add_match("interface='org.jackaudio.JackControl',member='ServerStopped'").unwrap();
            c.add_match("interface='org.jackaudio.JackControl',member='IsStarted'").unwrap();
            // also match jack_capture appear/disappear
            c.add_match("interface='org.jackaudio.JackPatchbay',member='ClientAppeared',arg2='jack_transport'").unwrap();
            c.add_match("interface='org.jackaudio.JackPatchbay',member='ClientAppeared',arg2='jack_capture'").unwrap();
            c.add_match("interface='org.jackaudio.JackPatchbay',member='ClientDisappeared',arg2='jack_capture'").unwrap();

            loop {

                if let Some(_) = c.incoming(1000).next() {
                    txur2
                        .send(Task {
                            id: id2.clone(),
                            update_time: Instant::now(),
                        })
                        .unwrap();
                }

                thread::sleep(Duration::new(0, 250_000_000))
            }
        });

        Ok(())
    }
}

pub struct Jack {
    text: TextWidget,
    id: String,
    device: Box<dyn SoundDevice>,
    config: Config,
    show_volume_when_muted: bool,
}

#[derive(Deserialize, Debug, Default, Clone)]
#[serde(deny_unknown_fields)]
pub struct SoundConfig {
    /// ALSA / PulseAudio sound device name
    #[serde(default = "SoundDriver::default")]
    pub driver: SoundDriver,

    /// ALSA / PulseAudio sound device name
    #[serde(default = "SoundConfig::default_name")]
    pub name: Option<String>,

    #[serde(default = "SoundConfig::default_show_volume_when_muted")]
    pub show_volume_when_muted: bool,

}

#[derive(Deserialize, Copy, Clone, Debug)]
#[serde(rename_all = "lowercase")]
pub enum SoundDriver {
    Auto,
    Alsa,
}

impl Default for SoundDriver {
    fn default() -> Self {
        SoundDriver::Auto
    }
}

impl SoundConfig {
    fn default_name() -> Option<String> {
        None
    }
    fn default_show_volume_when_muted() -> bool {
        false
    }
}

impl Jack {
    fn display(&mut self) -> Result<()> {
        self.device.get_info()?;

        let volume = self.device.volume();
        let running = self.device.jack_running();
        let rolling = self.device.jack_rolling();
        let capturing = self.device.jack_capturing();
        if self.device.muted() {
            self.text.set_icon("volume_empty");
            let icon = self
                .config
                .icons
                .get("volume_muted")
                .block_error("sound", "cannot find icon")?
                .to_owned();
            if self.show_volume_when_muted {
                self.text.set_text(format!("{} {:02}%", icon, volume));
            } else {
                self.text.set_text(icon);
            }
            self.text.set_state(State::Warning);
        } else {
            self.text.set_icon(match volume {
                0..=20 => "volume_empty",
                21..=70 => "volume_half",
                _ => "volume_full",
            });
            self.text.set_text(
                format!(
                    "{} {:02}% {}{}", 
                    if running { "JACK"} else { "ALSA"}, 
                    volume,
                    if capturing{ REC_ICON} else { ""},
                    if running {if rolling{ PLAY_ICON} else { STOP_ICON}} else {""}
                )
            );
            self.text.set_state(State::Idle);
        }

        Ok(())
    }
}

impl ConfigBlock for Jack {
    type Config = SoundConfig;

    fn new(
        block_config: Self::Config,
        config: Config,
        tx_update_request: Sender<Task>,
    ) -> Result<Self> {
        let id = Uuid::new_v4().to_simple().to_string();

        let device: Box<dyn SoundDevice> =  Box::new(JackSoundDevice::new(
                block_config.name.unwrap_or_else(|| "Master".into()),
            )?);


        let mut sound = Self {
            text: TextWidget::new(config.clone()).with_icon("volume_empty"),
            id: id.clone(),
            device,
            config,
            show_volume_when_muted: block_config.show_volume_when_muted,
        };

        sound
            .device
            .monitor(id.clone(), tx_update_request.clone())?;

        Ok(sound)
    }
}

// To filter [100%] output from amixer into 100
const FILTER: &[char] = &['[', ']', '%'];
const REC_ICON: &'static str = "  ";
const PLAY_ICON: &'static str = "  ";
const STOP_ICON: &'static str = "  ";

impl Block for Jack {
    fn update(&mut self) -> Result<Option<Duration>> {
        self.display()?;
        Ok(None) // The monitor thread will call for updates when needed.
    }

    fn view(&self) -> Vec<&dyn I3BarWidget> {
        vec![&self.text]
    }

    fn id(&self) -> &str {
        &self.id
    }
}
