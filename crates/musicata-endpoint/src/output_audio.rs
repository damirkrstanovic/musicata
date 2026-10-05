// SPDX-License-Identifier: AGPL-3.0-or-later
//! A separate audio channel: downloads on the transport thread cannot starve DSP updates.
use crate::{
    Creds,
    dsp::{Control, Registration, Update},
};
use musicata_core::{
    dsp::{DspProfile, OutputDspState},
    pcm_dsp::StereoEq,
};
use serde::Deserialize;
use std::{
    io::ErrorKind,
    net::{TcpStream, ToSocketAddrs},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc::{Receiver, SyncSender, TrySendError},
    },
    time::Duration,
};
use tungstenite::{Message, WebSocket};

#[derive(Clone, Deserialize)]
struct Config {
    state: OutputDspState,
    profile: Option<DspProfile>,
    #[serde(default)]
    meter_subscribed: bool,
}
struct Target {
    rate: u32,
    updates: SyncSender<Update>,
    sent: u64,
    alive: Arc<AtomicBool>,
}

pub fn start(creds: &Creds, control: Control, registrations: Receiver<Registration>) {
    let server = creds.server.clone();
    let id = creds.id.clone();
    let token = creds.token.clone();
    std::thread::spawn(move || {
        let mut targets = Vec::<Target>::new();
        let mut config: Option<Config> = None;
        let mut generation = 0;
        loop {
            let result = session(
                &server,
                &id,
                &token,
                &control,
                &registrations,
                &mut targets,
                &mut config,
                &mut generation,
            );
            if let Err(error) = result {
                eprintln!("audio channel: {error}");
            }
            std::thread::sleep(Duration::from_secs(2));
        }
    });
}
fn send(socket: &mut WebSocket<TcpStream>, value: serde_json::Value) -> anyhow::Result<()> {
    socket.send(Message::Text(value.to_string()))?;
    Ok(())
}
fn session(
    server: &str,
    id: &str,
    token: &str,
    control: &Control,
    registrations: &Receiver<Registration>,
    targets: &mut Vec<Target>,
    config: &mut Option<Config>,
    generation: &mut u64,
) -> anyhow::Result<()> {
    let (address, url) = crate::ws_target(server, id, token)?;
    let address = address
        .to_socket_addrs()?
        .next()
        .ok_or_else(|| anyhow::anyhow!("server address unavailable"))?;
    let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
    stream.set_read_timeout(Some(Duration::from_secs(2)))?;
    stream.set_write_timeout(Some(Duration::from_secs(2)))?;
    let (mut socket, _) = tungstenite::client(url.replace("/ws?", "/audio/ws?"), stream)?;
    socket
        .get_ref()
        .set_read_timeout(Some(Duration::from_millis(100)))?;
    send(&mut socket, serde_json::json!({"type":"renderer"}))?;
    let mut granted = false;
    let mut acknowledged = 0;
    let mut sequence = 0;
    let mut last_sample = 0;
    loop {
        match socket.read() {
            Ok(Message::Text(text)) => {
                let value: serde_json::Value = serde_json::from_str(&text)?;
                if value["type"] == "renderer_denied" {
                    anyhow::bail!("another renderer owns this output");
                }
                if value["type"] == "renderer_granted" {
                    granted = true;
                }
                if granted && !value["config"].is_null() {
                    let incoming: Config = serde_json::from_value(value["config"].clone())?;
                    let changed = config.as_ref().is_none_or(|old| {
                        old.state.session_id != incoming.state.session_id
                            || old.state.desired_revision != incoming.state.desired_revision
                    });
                    if changed {
                        *generation += 1;
                        sequence = 0;
                        let profile = if incoming.state.selection.enabled {
                            incoming
                                .profile
                                .clone()
                                .map(Some)
                                .ok_or("selected profile unavailable")
                        } else {
                            Ok(None)
                        };
                        *control
                            .desired
                            .lock()
                            .map_err(|_| anyhow::anyhow!("audio control stopped"))? =
                            Some((*generation, profile));
                    }
                    *config = Some(incoming);
                }
            }
            Ok(Message::Close(_)) => return Ok(()),
            Ok(_) => {}
            Err(tungstenite::Error::Io(error))
                if matches!(error.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => {}
            Err(error) => return Err(error.into()),
        }
        while let Ok(registration) = registrations.try_recv() {
            targets.push(Target {
                rate: registration.rate,
                updates: registration.updates,
                sent: registration.revision,
                alive: registration.alive,
            });
        }
        let Some(config) = config.as_ref().filter(|_| granted) else {
            continue;
        };
        let mut error = None;
        targets.retain_mut(|target| {
            if !target.alive.load(Ordering::Acquire) {
                return false;
            }
            if target.sent == *generation {
                return true;
            }
            let eq = if config.state.selection.enabled {
                config
                    .profile
                    .as_ref()
                    .ok_or("selected profile unavailable")
                    .and_then(|profile| StereoEq::from_profile(profile, target.rate))
            } else {
                Ok(None)
            };
            let eq = match eq {
                Ok(eq) => eq,
                Err(message) => {
                    error = Some(message);
                    return true;
                }
            };
            match target.updates.try_send(Update {
                revision: *generation,
                eq,
            }) {
                Ok(()) => {
                    target.sent = *generation;
                    true
                }
                Err(TrySendError::Full(_)) => true,
                Err(TrySendError::Disconnected(_)) => false,
            }
        });
        let applied = control.tap.applied_revision.load(Ordering::Acquire);
        if acknowledged != *generation && (applied == *generation || error.is_some()) {
            send(
                &mut socket,
                serde_json::json!({"type":"dsp_applied","session_id":config.state.session_id,"revision":config.state.desired_revision,"error":error}),
            )?;
            acknowledged = *generation;
        }
        if config.meter_subscribed
            && applied == *generation
            && control.playing.load(Ordering::Acquire)
        {
            if let Some((sample, levels)) = control
                .tap
                .read()
                .filter(|(sample, _)| *sample != last_sample)
            {
                last_sample = sample;
                sequence += 1;
                send(
                    &mut socket,
                    serde_json::json!({"type":"levels","session_id":config.state.session_id,"revision":config.state.desired_revision,"sequence":sequence,"levels":levels}),
                )?;
            }
        }
    }
}
