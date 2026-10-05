// SPDX-License-Identifier: AGPL-3.0-or-later
//! Stereo processing without locks, allocation or I/O in the sample callback.
use musicata_core::pcm_dsp::{AudioTap, StereoEq, StereoMeter};
use rodio::Source;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
        mpsc::{self, Receiver, SyncSender},
    },
    time::Duration,
};

pub struct Update {
    pub revision: u64,
    pub eq: Option<StereoEq>,
}
pub struct Registration {
    pub rate: u32,
    pub updates: SyncSender<Update>,
    pub alive: Arc<AtomicBool>,
    pub revision: u64,
}
#[derive(Clone)]
pub struct Control {
    pub tap: Arc<AudioTap>,
    pub playing: Arc<AtomicBool>,
    pub desired: Arc<
        Mutex<
            Option<(
                u64,
                Result<Option<musicata_core::dsp::DspProfile>, &'static str>,
            )>,
        >,
    >,
    volume: Arc<AtomicU64>,
    registrations: SyncSender<Registration>,
}
impl Control {
    pub fn new() -> (Self, Receiver<Registration>) {
        let (registrations, receiver) = mpsc::sync_channel(8);
        (
            Self {
                tap: Arc::default(),
                playing: Arc::default(),
                desired: Arc::default(),
                volume: Arc::new(AtomicU64::new(1.0f64.to_bits())),
                registrations,
            },
            receiver,
        )
    }
    pub fn set_volume(&self, volume: u8) {
        self.volume.store(
            (f64::from(volume.min(100)) / 100.0).to_bits(),
            Ordering::Relaxed,
        );
    }
    pub fn set_playing(&self, playing: bool) {
        self.playing.store(playing, Ordering::Release);
    }
}

pub struct DspSource<S> {
    source: S,
    control: Control,
    updates: Receiver<Update>,
    eq: Option<StereoEq>,
    meter: StereoMeter,
    frames: u32,
    right: Option<f32>,
    channels: u16,
    alive: Arc<AtomicBool>,
    initial_revision: u64,
}
impl<S: Source<Item = f32>> DspSource<S> {
    pub fn new(source: S, control: Control) -> Result<Self, &'static str> {
        let channels = source.channels();
        if !(1..=2).contains(&channels) {
            return Err("only mono and stereo outputs are supported");
        }
        let desired = control
            .desired
            .lock()
            .map_err(|_| "audio control stopped")?
            .clone();
        let (revision, profile) = desired.unwrap_or((0, Ok(None)));
        let profile = profile?;
        let eq = match profile.as_ref() {
            Some(profile) => StereoEq::from_profile(profile, source.sample_rate())?,
            None => None,
        };
        let alive = Arc::new(AtomicBool::new(true));
        let (sender, updates) = mpsc::sync_channel(1);
        control
            .registrations
            .try_send(Registration {
                rate: source.sample_rate(),
                updates: sender,
                alive: alive.clone(),
                revision,
            })
            .map_err(|_| "audio control worker is unavailable or busy")?;
        Ok(Self {
            source,
            control,
            updates,
            eq,
            meter: StereoMeter::default(),
            frames: 0,
            right: None,
            channels,
            alive,
            initial_revision: revision,
        })
    }
}
impl<S> Drop for DspSource<S> {
    fn drop(&mut self) {
        self.alive.store(false, Ordering::Release);
    }
}

impl<S: Source<Item = f32>> Iterator for DspSource<S> {
    type Item = f32;
    fn next(&mut self) -> Option<f32> {
        if let Some(right) = self.right.take() {
            return Some(right);
        }
        let mut applied = (self.frames == 0).then_some(self.initial_revision);
        if self.frames % 256 == 0 {
            if let Ok(update) = self.updates.try_recv() {
                self.eq = update.eq;
                self.meter.reset();
                applied = Some(update.revision);
            }
        }
        let left = f64::from(self.source.next()?);
        let right = if self.channels == 1 {
            left
        } else {
            f64::from(self.source.next()?)
        };
        if let Some(revision) = applied {
            self.control.tap.clear();
            self.control
                .tap
                .applied_revision
                .store(revision, Ordering::Release);
        }
        let (left, right) = self
            .eq
            .as_mut()
            .map_or((left, right), |eq| eq.process_frame(left, right));
        let gain = f64::from_bits(self.control.volume.load(Ordering::Relaxed));
        let finite = |value: f64| {
            if value.is_finite() {
                value.clamp(-1.0, 1.0) as f32
            } else {
                0.0
            }
        };
        let left = finite(left * gain);
        let right = finite(right * gain);
        self.meter.push(f64::from(left), f64::from(right));
        self.frames += 1;
        if self.frames % (self.source.sample_rate() / 20).max(1) == 0 {
            if let Some(levels) = self.meter.take() {
                self.control.tap.publish(levels);
            }
        }
        self.right = Some(right);
        Some(left)
    }
}
impl<S: Source<Item = f32>> Source for DspSource<S> {
    fn current_frame_len(&self) -> Option<usize> {
        None
    }
    fn channels(&self) -> u16 {
        2
    }
    fn sample_rate(&self) -> u32 {
        self.source.sample_rate()
    }
    fn total_duration(&self) -> Option<Duration> {
        self.source.total_duration()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rodio::buffer::SamplesBuffer;

    #[test]
    fn replacement_sources_start_with_current_profile_at_their_sample_rate() {
        let (control, registrations) = Control::new();
        let profile = musicata_core::dsp::DspProfile {
            id: "test".into(),
            name: "Test".into(),
            preamp_db: -6.0,
            bands: vec![],
            kind: None,
            room_ir: None,
        };
        *control.desired.lock().unwrap() = Some((7, Ok(Some(profile))));
        for rate in [44100, 48000] {
            let mut source = DspSource::new(
                SamplesBuffer::new(2, rate, vec![0.5f32, 0.0]),
                control.clone(),
            )
            .unwrap();
            let registration = registrations.recv().unwrap();
            assert_eq!(registration.rate, rate);
            assert!(registration.alive.load(Ordering::Acquire));
            assert!((source.next().unwrap() - 0.2505936).abs() < 1e-6);
            assert_eq!(control.tap.applied_revision.load(Ordering::Acquire), 7);
            drop(source);
            assert!(!registration.alive.load(Ordering::Acquire));
        }
    }

    #[test]
    fn mono_maps_to_stereo_and_meters_final_gain() {
        let (control, registrations) = Control::new();
        control.set_volume(50);
        let input = SamplesBuffer::new(1, 48000, vec![0.5f32; 4800]);
        let mut source = DspSource::new(input, control.clone()).unwrap();
        let registration = registrations.recv().unwrap();
        registration
            .updates
            .send(Update {
                revision: 3,
                eq: None,
            })
            .unwrap();
        assert_eq!(source.channels(), 2);
        let samples: Vec<_> = source.by_ref().collect();
        assert_eq!(samples.len(), 9600);
        assert!(samples.iter().all(|x| (*x - 0.25).abs() < 1e-6));
        let (_, levels) = control.tap.read().unwrap();
        assert!((levels.rms_l - 0.25).abs() < 1e-6);
        assert_eq!(levels.peak_r, 0.25);
        assert_eq!(control.tap.applied_revision.load(Ordering::Acquire), 3);
    }

    #[test]
    fn live_update_preserves_cursor_and_channel_separation() {
        let (control, registrations) = Control::new();
        let input = SamplesBuffer::new(2, 44100, vec![0.5f32, 0.0].repeat(5000));
        let mut source = DspSource::new(input, control.clone()).unwrap();
        let registration = registrations.recv().unwrap();
        assert_eq!(registration.rate, 44100);
        for _ in 0..512 {
            source.next().unwrap();
        }
        let profile = musicata_core::dsp::DspProfile {
            id: "test".into(),
            name: "test".into(),
            preamp_db: -6.0,
            bands: vec![],
            kind: None,
            room_ir: None,
        };
        registration
            .updates
            .send(Update {
                revision: 4,
                eq: StereoEq::from_profile(&profile, 44100).unwrap(),
            })
            .unwrap();
        let remaining: Vec<_> = source.collect();
        assert_eq!(remaining.len(), 10000 - 512);
        assert!((remaining[0] - 0.2505936).abs() < 1e-6);
        assert_eq!(remaining[1], 0.0);
    }

    #[test]
    fn unsupported_channel_layout_is_rejected() {
        let (control, _) = Control::new();
        assert!(DspSource::new(SamplesBuffer::new(6, 48000, vec![0.0f32; 12]), control).is_err());
    }
}
