// SPDX-License-Identifier: AGPL-3.0-or-later
//! Optional output-host CamillaDSP integration. All network work runs off request/audio paths.
use crate::output_audio::AudioOutput;
use musicata_core::dsp::{DspProfile, DspStatus, StereoLevels};
use serde_json::{Value, json};
use std::{
    net::{TcpStream, ToSocketAddrs},
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tungstenite::{Message, WebSocket};
const STAGE: &str = "Musicata output correction";

fn correction_patch(
    active: &Value,
    profile: Option<&DspProfile>,
) -> anyhow::Result<(Value, Value)> {
    let rate = active["devices"]["samplerate"]
        .as_u64()
        .ok_or_else(|| anyhow::anyhow!("processor has no active sample rate"))?
        as u32;
    if active["devices"]["playback"]["channels"].as_u64() != Some(2) {
        anyhow::bail!("processor playback must be stereo");
    }
    if let Some(profile) = profile {
        musicata_core::pcm_dsp::StereoEq::from_profile(profile, rate)
            .map_err(anyhow::Error::msg)?;
    }
    let mut candidate = active.clone();
    let pipeline = candidate["pipeline"]
        .as_array_mut()
        .ok_or_else(|| anyhow::anyhow!("processor has no pipeline"))?;
    let removed: Vec<String> = pipeline
        .iter()
        .filter(|stage| stage["description"] == STAGE)
        .flat_map(|stage| {
            stage["names"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|name| name.as_str().map(str::to_owned))
        })
        .collect();
    pipeline.retain(|stage| stage["description"] != STAGE);
    // Reserved names may only be touched if the previous marked stage owned them.
    if pipeline.iter().any(|stage| {
        stage["names"].as_array().is_some_and(|names| {
            names.iter().any(|name| {
                name.as_str()
                    .is_some_and(|name| name.starts_with("musicata_"))
            })
        })
    }) {
        anyhow::bail!("another pipeline stage uses Musicata filter names");
    }
    let mut filters = serde_json::Map::new();
    filters.insert(
        "musicata_preamp".into(),
        json!({"type":"Gain","parameters":{"gain":profile.map_or(0.0,|p| p.preamp_db)}}),
    );
    let mut names = vec!["musicata_preamp".to_string()];
    if let Some(profile) = profile {
        for (index, band) in profile.bands.iter().enumerate() {
            let name = format!("musicata_band_{index}");
            let kind = match band.band_type.as_str() {
                "peaking" => "Peaking",
                "lowshelf" => "Lowshelf",
                "highshelf" => "Highshelf",
                _ => anyhow::bail!("unsupported band"),
            };
            // Web Audio shelves use fixed slope S=1, equivalent to CamillaDSP Q=1/sqrt(2).
            let q = if band.band_type == "peaking" {
                band.q
            } else {
                std::f64::consts::FRAC_1_SQRT_2
            };
            filters.insert(name.clone(),json!({"type":"Biquad","parameters":{"type":kind,"freq":band.freq,"gain":band.gain,"q":q}}));
            names.push(name);
        }
    }
    pipeline.push(json!({"type":"Filter","channels":[0,1],"names":names,"description":STAGE}));
    let pipeline = Value::Array(pipeline.clone());
    let existing = candidate["filters"]
        .as_object_mut()
        .ok_or_else(|| anyhow::anyhow!("processor has no filter map"))?;
    let mut patch_filters = filters.clone();
    for name in removed {
        existing.remove(&name);
        if !filters.contains_key(&name) {
            patch_filters.insert(name, Value::Null);
        }
    }
    for (name, filter) in filters {
        existing.insert(name, filter);
    }
    Ok((
        candidate,
        json!({"filters":patch_filters,"pipeline":pipeline}),
    ))
}

fn playback_levels(rms: &Value, peak: &Value) -> Option<StereoLevels> {
    let rms = rms.as_array()?;
    let peak = peak.as_array()?;
    if rms.len() != 2 || peak.len() != 2 {
        return None;
    }
    let linear = |value: &Value| {
        value
            .as_f64()
            .filter(|db| db.is_finite())
            .map(|db| 10f64.powf(db / 20.0))
    };
    let levels = StereoLevels {
        rms_l: linear(&rms[0])?,
        rms_r: linear(&rms[1])?,
        peak_l: linear(&peak[0])?,
        peak_r: linear(&peak[1])?,
    };
    levels.is_valid().then_some(levels)
}
fn reply_value(command: &str, reply: Value) -> anyhow::Result<Value> {
    let result = &reply[command];
    if result["result"] != "Ok" {
        anyhow::bail!("{command} failed: {}", result["value"]);
    }
    Ok(result["value"].clone())
}
// CamillaDSP serializes optional defaults in the active config. Compare every requested
// value while accepting those added defaults, rather than falsely reporting a failed patch.
fn config_matches(actual: &Value, expected: &Value) -> bool {
    match (actual, expected) {
        (Value::Object(actual), Value::Object(expected)) => expected.iter().all(|(key, value)| {
            actual
                .get(key)
                .is_some_and(|actual| config_matches(actual, value))
        }),
        (Value::Array(actual), Value::Array(expected)) => {
            actual.len() == expected.len()
                && actual
                    .iter()
                    .zip(expected)
                    .all(|(a, e)| config_matches(a, e))
        }
        (Value::Number(actual), Value::Number(expected)) => actual.as_f64() == expected.as_f64(),
        _ => actual == expected,
    }
}

struct Client {
    socket: WebSocket<TcpStream>,
}
impl Client {
    fn connect(host: &str, port: u16) -> anyhow::Result<Self> {
        let address = (host, port)
            .to_socket_addrs()?
            .next()
            .ok_or_else(|| anyhow::anyhow!("processor address unavailable"))?;
        let stream = TcpStream::connect_timeout(&address, Duration::from_secs(2))?;
        stream.set_read_timeout(Some(Duration::from_secs(2)))?;
        stream.set_write_timeout(Some(Duration::from_secs(2)))?;
        let authority = if host.contains(':') {
            format!("[{host}]:{port}")
        } else {
            format!("{host}:{port}")
        };
        let (socket, _) = tungstenite::client(format!("ws://{authority}/"), stream)?;
        Ok(Self { socket })
    }
    fn request(&mut self, command: &str, payload: Option<Value>) -> anyhow::Result<Value> {
        let request = payload.map_or_else(|| json!(command), |payload| json!({command:payload}));
        self.socket.send(Message::Text(request.to_string()))?;
        loop {
            match self.socket.read()? {
                Message::Text(text) => return reply_value(command, serde_json::from_str(&text)?),
                Message::Close(_) => anyhow::bail!("processor disconnected"),
                _ => {}
            }
        }
    }
    fn config(&mut self) -> anyhow::Result<Value> {
        let value = self.request("GetConfigJson", None)?;
        Ok(serde_json::from_str(value.as_str().ok_or_else(|| {
            anyhow::anyhow!("processor has no active config")
        })?)?)
    }
    fn apply(&mut self, profile: Option<&DspProfile>) -> anyhow::Result<()> {
        let active = self.config()?;
        let (candidate, patch) = correction_patch(&active, profile)?;
        self.request("ValidateConfigJson", Some(json!(candidate.to_string())))?;
        self.request("PatchConfig", Some(patch))?;
        for _ in 0..20 {
            let actual = self.config()?;
            let state = self.request("GetState", None)?;
            if actual["devices"] != active["devices"] {
                anyhow::bail!("processor devices changed during correction update");
            }
            if config_matches(&actual["pipeline"], &candidate["pipeline"])
                && config_matches(&actual["filters"], &candidate["filters"])
                && actual["filters"].as_object().map(|filters| filters.len())
                    == candidate["filters"]
                        .as_object()
                        .map(|filters| filters.len())
                && matches!(state.as_str(), Some("Running" | "Paused"))
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        anyhow::bail!("processor did not confirm active correction")
    }
}

pub fn start(output: Arc<AudioOutput>) {
    let weak = Arc::downgrade(&output);
    let mut configs = output.config.subscribe();
    let mut bindings = output.processor.subscribe();
    tokio::task::spawn_blocking(move || {
        let mut client: Option<Client> = None;
        let mut binding = None;
        let mut revision = 0;
        let mut sequence = 0;
        let mut failed = false;
        loop {
            std::thread::sleep(Duration::from_millis(100));
            let Some(output) = weak.upgrade() else { break };
            if output.closed.load(Ordering::Acquire) {
                break;
            }
            let desired_binding = bindings.borrow_and_update().clone();
            if binding != desired_binding {
                client = None;
                revision = 0;
                binding = desired_binding;
            }
            let config = configs.borrow_and_update().clone();
            let result =
                (|| -> anyhow::Result<()> {
                    let (host, port) = binding
                        .as_ref()
                        .ok_or_else(|| anyhow::anyhow!("No output processor is configured"))?;
                    if client.is_none() {
                        client = Some(Client::connect(host, *port)?);
                        revision = 0;
                    }
                    let client = client.as_mut().unwrap();
                    if revision != config.state.desired_revision {
                        let profile =
                            if config.state.selection.enabled {
                                Some(config.profile.as_ref().ok_or_else(|| {
                                    anyhow::anyhow!("selected profile unavailable")
                                })?)
                            } else {
                                None
                            };
                        client.apply(profile)?;
                        let mut runtime = output.runtime.blocking_lock();
                        runtime.claim(0);
                        runtime.state.capabilities.peq = true;
                        runtime.state.capabilities.meter = true;
                        runtime.acknowledge(0, config.state.desired_revision, None);
                        revision = config.state.desired_revision;
                        output
                            .config
                            .send_modify(|config| config.state = runtime.state.clone());
                    }
                    if output.subscribers.load(Ordering::Acquire) > 0 {
                        let rms = client.request("GetPlaybackSignalRms", None)?;
                        let peak = client.request("GetPlaybackSignalPeak", None)?;
                        let levels = playback_levels(&rms, &peak);
                        let mut runtime = output.runtime.blocking_lock();
                        if let Some(levels) = levels {
                            sequence += 1;
                            runtime.accept_levels(0, revision, sequence, levels);
                        } else {
                            runtime.last_levels = None;
                        }
                    }
                    Ok(())
                })();
            if result.is_ok() && failed {
                failed = false;
                let id = output.runtime.blocking_lock().state.output_id.clone();
                crate::diagnostics::recovery("dsp.processor", "camilladsp", Some(&id));
            }
            if let Err(error) = result {
                failed = true;
                client = None;
                let mut runtime = output.runtime.blocking_lock();
                let message = error.to_string();
                crate::diagnostics::failure(
                    "dsp.processor",
                    "camilladsp",
                    Some(&runtime.state.output_id),
                    &message,
                );
                let changed = runtime.state.error.as_deref() != Some(&message);
                runtime.last_levels = None;
                runtime.state.status = DspStatus::Unavailable;
                runtime.state.applied_revision = None;
                runtime.state.error = Some(message);
                if binding.is_none() {
                    runtime.state.capabilities.peq = false;
                    runtime.state.capabilities.meter = false;
                }
                if changed {
                    output
                        .config
                        .send_modify(|config| config.state = runtime.state.clone());
                }
                drop(runtime);
                drop(output);
                std::thread::sleep(Duration::from_secs(2));
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    #[test]
    #[ignore = "requires a CamillaDSP 4.1.x executable; run explicitly in the development cluster"]
    fn camilla_real_pcm_confirms_live_patch_and_shelf_parity() {
        use std::{
            io::Write,
            process::{Command, Stdio},
            sync::atomic::{AtomicBool, Ordering},
        };
        let executable = std::env::var("MUSICATA_TEST_CAMILLA").expect("set MUSICATA_TEST_CAMILLA");
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let dir =
            std::env::temp_dir().join(format!("musicata-camilla-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        struct Probe {
            child: std::process::Child,
            stop: Arc<AtomicBool>,
            dir: std::path::PathBuf,
        }
        impl Drop for Probe {
            fn drop(&mut self) {
                self.stop.store(true, Ordering::Release);
                let _ = self.child.kill();
                let _ = self.child.wait();
                let _ = std::fs::remove_dir_all(&self.dir);
            }
        }
        let child = Command::new(executable)
            .args(["-w", "-a", "127.0.0.1", "-p", &port.to_string()])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .spawn()
            .unwrap();
        let mut probe = Probe {
            child,
            stop: Arc::new(AtomicBool::new(false)),
            dir: dir.clone(),
        };
        let mut client = None;
        for _ in 0..100 {
            if let Ok(connected) = Client::connect("127.0.0.1", port) {
                client = Some(connected);
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        let mut client = client.expect("processor starts");
        let initial = json!({"devices":{"samplerate":48000,"chunksize":960,"queuelimit":4,"silence_threshold":-100,"silence_timeout":0,"capture":{"type":"Stdin","channels":2,"format":"S16_LE"},"playback":{"type":"File","filename":dir.join("pcm.raw"),"channels":2,"format":"S16_LE"}},"filters":{},"pipeline":[]});
        client
            .request("ValidateConfigJson", Some(json!(initial.to_string())))
            .unwrap();
        client
            .request("SetConfigJson", Some(json!(initial.to_string())))
            .unwrap();
        let mut stdin = probe.child.stdin.take().unwrap();
        let stop = probe.stop.clone();
        std::thread::spawn(move || {
            let mut pcm = Vec::new();
            for frame in 0..960 {
                let x = (std::f64::consts::TAU * 1000.0 * frame as f64 / 48000.0).sin();
                for gain in [0.1, 0.05] {
                    pcm.extend_from_slice(&((x * gain * 32768.0).round() as i16).to_le_bytes());
                }
            }
            while !stop.load(Ordering::Acquire) {
                if stdin.write_all(&pcm).is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        std::thread::sleep(Duration::from_millis(300));
        let devices = client.config().unwrap()["devices"].clone();
        let mut profile = DspProfile {
            id: "probe".into(),
            name: "Probe".into(),
            preamp_db: -6.0,
            bands: vec![musicata_core::dsp::DspBand {
                band_type: "peaking".into(),
                freq: 1000.0,
                gain: 6.0,
                q: 1.0,
            }],
            kind: None,
            room_ir: None,
        };
        client.apply(Some(&profile)).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let rms = client.request("GetPlaybackSignalRms", None).unwrap();
        let peak = client.request("GetPlaybackSignalPeak", None).unwrap();
        let levels = playback_levels(&rms, &peak).unwrap();
        assert!((levels.peak_l - 0.1).abs() < 0.002);
        assert!((levels.peak_r - 0.05).abs() < 0.002);
        for kind in ["lowshelf", "highshelf"] {
            profile.bands[0].band_type = kind.into();
            profile.bands[0].freq = 1000.0;
            profile.bands[0].q = 0.2;
            client.apply(Some(&profile)).unwrap();
            std::thread::sleep(Duration::from_millis(400));
            let levels = playback_levels(
                &client.request("GetPlaybackSignalRms", None).unwrap(),
                &client.request("GetPlaybackSignalPeak", None).unwrap(),
            )
            .unwrap();
            let mut eq = musicata_core::pcm_dsp::StereoEq::from_profile(&profile, 48000)
                .unwrap()
                .unwrap();
            let mut meter = musicata_core::pcm_dsp::StereoMeter::default();
            for frame in 0..9600 {
                let x = (std::f64::consts::TAU * 1000.0 * frame as f64 / 48000.0).sin();
                let (l, r) = eq.process_frame(x * 0.1, x * 0.05);
                if frame >= 4800 {
                    meter.push(l, r);
                }
            }
            let expected = meter.take().unwrap();
            assert!(
                (levels.rms_l - expected.rms_l).abs() < 0.002,
                "{kind}: {levels:?} vs {expected:?}"
            );
            assert!((levels.rms_r - expected.rms_r).abs() < 0.002);
        }
        client.apply(None).unwrap();
        std::thread::sleep(Duration::from_millis(300));
        let levels = playback_levels(
            &client.request("GetPlaybackSignalRms", None).unwrap(),
            &client.request("GetPlaybackSignalPeak", None).unwrap(),
        )
        .unwrap();
        assert!((levels.peak_l - 0.1).abs() < 0.002);
        assert_eq!(client.config().unwrap()["devices"], devices);
        assert!(
            client.config().unwrap()["filters"]
                .get("musicata_band_0")
                .is_none()
        );
    }

    #[test]
    fn active_confirmation_accepts_defaults_but_rejects_changed_filter() {
        let desired = json!({"filters":{"gain":{"type":"Gain","parameters":{"gain":-6}}},"pipeline":[{"type":"Filter","channels":[0,1],"names":["gain"]}]});
        let mut actual = json!({"filters":{"gain":{"type":"Gain","parameters":{"gain":-6,"inverted":false,"scale":"dB"}}},"pipeline":[{"type":"Filter","channels":[0,1],"names":["gain"],"bypassed":false}]});
        assert!(config_matches(&actual, &desired));
        actual["filters"]["gain"]["parameters"]["gain"] = json!(-3);
        assert!(!config_matches(&actual, &desired));
    }

    #[test]
    fn websocket_client_handles_success_and_protocol_failure() {
        use std::net::TcpListener;
        for reply in [
            json!({"GetState":{"result":"Ok","value":"Running"}}),
            json!({"GetState":{"result":"Error","value":"no config"}}),
        ] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            let expected = reply["GetState"]["result"] == "Ok";
            let server = std::thread::spawn(move || {
                let (stream, _) = listener.accept().unwrap();
                let mut socket = tungstenite::accept(stream).unwrap();
                assert_eq!(socket.read().unwrap().into_text().unwrap(), "\"GetState\"");
                socket.send(Message::Text(reply.to_string())).unwrap();
            });
            let mut client = Client::connect("127.0.0.1", port).unwrap();
            assert_eq!(client.request("GetState", None).is_ok(), expected);
            server.join().unwrap();
        }
    }

    #[test]
    fn websocket_client_timeout_does_not_hang() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().unwrap();
            let mut socket = tungstenite::accept(stream).unwrap();
            socket.read().unwrap();
            std::thread::sleep(Duration::from_millis(200));
        });
        let mut client = Client::connect("127.0.0.1", port).unwrap();
        client
            .socket
            .get_ref()
            .set_read_timeout(Some(Duration::from_millis(30)))
            .unwrap();
        let start = std::time::Instant::now();
        assert!(client.request("GetState", None).is_err());
        assert!(start.elapsed() < Duration::from_secs(1));
        server.join().unwrap();
    }

    #[test]
    fn patch_preserves_devices_and_foreign_stages_and_removes_owned_bands() {
        let config = json!({"devices":{"samplerate":48000,"capture":{"channels":2},"playback":{"channels":2}},"filters":{"foreign":{"type":"Gain","parameters":{"gain":-2}},"musicata_band_0":{}},"pipeline":[{"type":"Filter","channels":[0,1],"names":["foreign"]},{"type":"Filter","channels":[0,1],"names":["musicata_band_0"],"description":"Musicata output correction"}]});
        let (candidate, patch) = correction_patch(&config, None).unwrap();
        assert_eq!(candidate["devices"], config["devices"]);
        assert_eq!(candidate["pipeline"][0], config["pipeline"][0]);
        assert!(candidate["filters"].get("musicata_band_0").is_none());
        assert!(patch["filters"]["musicata_band_0"].is_null());
        assert_eq!(
            candidate["filters"]["musicata_preamp"]["parameters"]["gain"],
            0.0
        );
    }
    #[test]
    fn playback_db_is_converted_once_and_invalid_channels_rejected() {
        let levels = playback_levels(
            &json!([-6.020599913, -12.041199826]),
            &json!([0.0, -6.020599913]),
        )
        .unwrap();
        assert!((levels.rms_l - 0.5).abs() < 1e-9);
        assert!((levels.rms_r - 0.25).abs() < 1e-9);
        assert!((levels.peak_l - 1.0).abs() < 1e-9);
        assert!(playback_levels(&json!([]), &json!([])).is_none());
        assert!(playback_levels(&json!([0]), &json!([0])).is_none());
    }
    #[test]
    fn protocol_errors_and_wrong_reply_are_rejected() {
        assert_eq!(
            reply_value(
                "GetState",
                json!({"GetState":{"result":"Ok","value":"Running"}})
            )
            .unwrap(),
            json!("Running")
        );
        assert!(
            reply_value(
                "GetState",
                json!({"GetState":{"result":"Error","value":"no config"}})
            )
            .is_err()
        );
        assert!(
            reply_value(
                "GetState",
                json!({"GetConfigJson":{"result":"Ok","value":"{}"}})
            )
            .is_err()
        );
    }
}
