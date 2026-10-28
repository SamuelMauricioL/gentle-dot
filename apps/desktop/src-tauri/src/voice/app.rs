//! The app side of voice input (S30.1, L88) and of the optional local model (S30.5): the
//! `voice_*` commands and the `voice://*` events. Recording events go only to the webview that
//! started the recording; `voice://model` events go to the panel. On macOS, Apple Speech records
//! until the local model is installed; on Linux, the local model is the only engine; elsewhere
//! `voice_status` answers "unavailable" and the other recording commands fail. Every command
//! runs on a blocking thread: starting may wait on a permission prompt, stopping waits for the
//! final transcript, and removing the model deletes files.

use super::engine::{fallback_engine, Preferred};
use super::model::{HttpFetcher, ModelError, ModelEvent, ModelEvents, ModelManager, ModelSpec, ModelStatus, MODEL_EVENT};
use super::parakeet::{self, Loader, Locate, Parakeet};
use super::wake::{self, Chime, WakeWord};
use super::{ListenEvent, ListenEvents};
use crate::config::{self, VoiceInput as StoredInput};
use super::{Events, Transcript, Voice, VoiceError, VoiceEvent, VoiceStatus, UNSUPPORTED};
use crate::computer::SystemClock;
use crate::config::DesktopConfig;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};

/// How long each retry waits before trying the microphone again: a second, then doubling.
const RETRY_MS: [u64; 4] = [1_000, 2_000, 4_000, 8_000];
use tauri::{AppHandle, Emitter, EventTarget, Manager};

/// How long a note the wake word started may hold the microphone. The auto-stop counts the audio it
/// receives, so a capture that delivers none would never end, and the detector would stay paused
/// with the switch still on: the wall clock ends it instead (S1, S4).
const NOTE_DEADLINE_MS: u64 = 65_000;

/// How long a note may spend starting before it is treated as stalled (S1, S4): a Bluetooth input
/// whose engine never delivers leaves the machine in `Starting` and the detector paused.
const STALL_MS: u64 = 3_000;

/// The listen channel is watched on its own clock: a capture that opened and delivers nothing is a
/// lost channel whatever the engine reported (screenpipe's `Ok` only means "no error, not
/// capturing"), so it is reopened with backoff and, after the retries, on the system default input
/// rather than left silent.
const CAPTURE_TICK_MS: u64 = 2_000;
const CAPTURE_SILENT_MS: u64 = 8_000;
const CAPTURE_RETRIES: usize = 3;

/// How long the supervisor leaves the microphone alone after every input failed to deliver.
const CAPTURE_HOLD_MS: u64 = 60_000;

/// The last link of the chain: when neither the chosen input nor the system default delivers
/// anything (a Bluetooth headset that slept, a device that went away), the built-in microphone
/// keeps the wake word alive. It is named by UID so it is used whether or not it is the default.
#[cfg(target_os = "macos")]
const BUILT_IN_INPUT: &str = "BuiltInMicrophoneDevice";


/// The commands, as registered in `build.rs` and granted to the panel.
pub const COMMANDS: [&str; 13] = [
    "voice_status",
    "voice_start",
    "voice_stop",
    "voice_cancel",
    "voice_input_options",
    "voice_input_set",
    "voice_model_status",
    "voice_model_download",
    "voice_model_cancel",
    "voice_model_remove",
    "wake_word_status",
    "wake_word_set",
    "wake_phrase_set",
];

/// Said when the panel asks to record from a microphone that is not one.
pub const EMPTY_INPUT: &str = "Choose a microphone to record from.";


/// How long the panel waits for a burst of device notifications to settle before it is told (S1):
/// a Bluetooth headset flips between its two profiles several times a second (S2), and every flip
/// is a notification.
const DEVICES_SETTLE_MS: u64 = 300;

/// The platform's input devices (UID and display name) and the UID of its default input. A
/// function rather than a call so the tests answer fixed devices instead of this Mac's.
type Enumerate = fn() -> (Vec<StoredInput>, Option<String>);

/// Where a changed microphone list goes: the panel's `voice://devices` (S1, S2).
type DevicesEvents = Arc<dyn Fn(&InputOptions) + Send + Sync>;

/// Installs the system's device notifications, calling back on every change, and hands back the
/// registration whose drop removes it (S4). A closure rather than a call for the same reason as
/// `Enumerate`: the tests drive the notifications instead of opening a real CoreAudio listener.
type Watch =
    Arc<dyn Fn(Arc<dyn Fn() + Send + Sync>) -> Result<Option<Box<dyn Send + Sync>>, String> + Send + Sync>;

/// macOS asks CoreAudio (S1-S4); elsewhere the system default input is all there is, so the
/// panel gets an empty list rather than a device it could never select (S7, Linux).
fn platform_devices() -> (Vec<StoredInput>, Option<String>) {
    #[cfg(target_os = "macos")]
    return (
        super::core_audio::inputs().into_iter().map(|device| StoredInput { uid: device.uid, name: device.name }).collect(),
        super::core_audio::default_input_uid(),
    );
    #[cfg(not(target_os = "macos"))]
    (Vec::new(), None)
}

/// macOS: the CoreAudio listener (S2). Elsewhere no notification exists, so nothing is watched and
/// the list stays what it was: there is only one input there and nothing to switch to.
fn platform_watch(changed: Arc<dyn Fn() + Send + Sync>) -> Result<Option<Box<dyn Send + Sync>>, String> {
    #[cfg(target_os = "macos")]
    return super::core_audio::DeviceListener::install(changed)
        .map(|listener| Some(Box::new(listener) as Box<dyn Send + Sync>));
    #[cfg(not(target_os = "macos"))]
    {
        let _ = changed;
        Ok(None)
    }
}

/// The `voice_input_options` reply, out of what the system reports and what is stored: one
/// function, so the pushed list and the answered one cannot disagree (S1).
fn options(devices: Enumerate, stored: &Mutex<Option<StoredInput>>) -> InputOptions {
    let (devices, default) = devices();
    // The stored choice stays in the answer even when the device is gone, so the panel can warn
    // about it (S3); a recording from it falls back to the default.
    let chosen = stored.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(|input| input.uid.clone());
    InputOptions { devices, chosen, default }
}

/// The choice in `config.json`, read once at startup, and the file it is written back to.
fn stored_input() -> (PathBuf, Option<StoredInput>) {
    let config = config::load_config();
    (config.data_dir.join("config.json"), config.voice_input)
}

/// The `voice_input_options` reply: `{devices, chosen, default}`, the two choices as UIDs.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct InputOptions {
    /// Every input the Mac reports, in its own order (empty off macOS).
    pub devices: Vec<StoredInput>,
    /// The stored UID, or `null` when following the system default.
    pub chosen: Option<String>,
    /// The system default input's UID, or `null` while it is not known.
    pub default: Option<String>,
}

/// The `voice_input_set` reply: `{chosen}`, the UID that is now stored, or `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct InputChoice {
    pub chosen: Option<String>,
}

/// The watch that keeps the panel's list in step with the system (S1, S2): the registration the
/// system notifies, and the channel that keeps its worker alive. Both are held for what dropping
/// them does, which is the shutdown (S4).
struct DevicesWatch {
    /// Listed first so it goes first: the listener is removed before the channel it pokes closes.
    _registration: Box<dyn Send + Sync>,
    /// Open for as long as the watch lives: closing it is how the worker is told to leave.
    _pokes: mpsc::Sender<()>,
}

/// The worker behind `voice://devices` (S1, S2): waits for a notification, lets the burst settle,
/// reads the list again and pushes it only when it differs from the last one pushed.
fn follow_devices(
    devices: Enumerate,
    stored: Arc<Mutex<Option<StoredInput>>>,
    events: DevicesEvents,
    asked: mpsc::Receiver<()>,
    mut last: InputOptions,
) {
    let settle = Duration::from_millis(DEVICES_SETTLE_MS);
    loop {
        // A closed channel is the watch being dropped, which is the app going away (S4).
        if asked.recv().is_err() {
            return;
        }
        // The wait restarts with every notification, so a device that announces itself eight times
        // in as many hundreds of milliseconds is read once, settled (S1).
        let mut changed_at = Instant::now();
        loop {
            let left = settle.saturating_sub(changed_at.elapsed());
            if left.is_zero() {
                break;
            }
            match asked.recv_timeout(left) {
                Ok(()) => changed_at = Instant::now(),
                Err(mpsc::RecvTimeoutError::Timeout) => break,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
        let current = options(devices, &stored);
        // Byte for byte the list the panel already shows: it is not told, so it does not re-render.
        if current == last {
            continue;
        }
        last = current.clone();
        events(&current);
    }
}

/// Voice input, when this platform has a backend, and the wake word that listens on the same
/// microphone (S30.7).
pub struct VoiceInput {
    voice: Option<Voice>,
    wake: Option<Wake>,
    /// `config.json`, where the microphone choice is kept between recordings and launches (S2).
    config: PathBuf,
    /// The stored microphone; `None` follows the system default input. Shared with the watch, so
    /// a pushed list carries the choice the recording will read (S1).
    stored: Arc<Mutex<Option<StoredInput>>>,
    /// The inputs this platform offers and its default, read when the panel asks.
    devices: Enumerate,
    /// How this platform is told that its devices changed (S1, S2).
    watch: Watch,
    /// The watch itself, once it is installed: the listener and the worker behind it (S4).
    watched: Mutex<Option<DevicesWatch>>,
}

impl VoiceInput {
    pub fn new(voice: Voice) -> Self {
        let (config, stored) = stored_input();
        VoiceInput {
            voice: Some(voice),
            wake: None,
            config,
            stored: Arc::new(Mutex::new(stored)),
            devices: platform_devices,
            watch: Arc::new(platform_watch),
            watched: Mutex::new(None),
        }
    }

    pub fn unsupported() -> Self {
        let (config, stored) = stored_input();
        VoiceInput {
            voice: None,
            wake: None,
            config,
            stored: Arc::new(Mutex::new(stored)),
            devices: platform_devices,
            watch: Arc::new(platform_watch),
            watched: Mutex::new(None),
        }
    }

    fn voice(&self) -> Result<&Voice, VoiceError> {
        self.voice.as_ref().ok_or(VoiceError::Unsupported)
    }

    /// The stored device UID; `None` follows the system default input.
    fn chosen(&self) -> Option<String> {
        self.stored.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).as_ref().map(|input| input.uid.clone())
    }

    /// The devices the panel may choose from, the stored choice, and the system default.
    pub fn input_options(&self) -> InputOptions {
        options(self.devices, &self.stored)
    }

    /// Keeps the panel's microphone list in step with the system instead of leaving it the snapshot
    /// it read when it opened (S1, S2): the whole reply is pushed once now, and again after every
    /// change CoreAudio reports, once the change has settled and only if the list really differs. A
    /// listener that cannot be installed is logged with its reason, so a list that stops following
    /// the system is known instead of silent (S4).
    pub fn watch_devices(&mut self, events: DevicesEvents) {
        let first = self.input_options();
        // The list as it is now, before anything can change it: the panel's own read of it may be
        // older than this process.
        events(&first);
        let (pokes, asked) = mpsc::channel();
        let kept = pokes.clone();
        let notify: Arc<dyn Fn() + Send + Sync> = Arc::new(move || {
            // A send that fails is a worker that already left: nothing left to tell it.
            let _ = pokes.send(());
        });
        let registration = match (self.watch)(notify) {
            Ok(Some(registration)) => registration,
            // S4: the list will not follow the system, and this is why.
            Err(reason) => return eprintln!("gentle-dot: the microphone list will not follow the system: {reason}"),
            // Nothing on this platform notifies: there is no worker to poke.
            Ok(None) => return,
        };
        let (devices, stored) = (self.devices, Arc::clone(&self.stored));
        std::thread::spawn(move || follow_devices(devices, stored, events, asked, first));
        *self.watched.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) =
            Some(DevicesWatch { _registration: registration, _pokes: kept });
    }

    /// Stops following the system (S4): the listener is removed, so no notification outlives the
    /// app, and the worker ends with the channel it waits on.
    pub fn stop_watching_devices(&self) {
        drop(self.watched.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).take());
    }

    /// Stores the choice (a device UID, or `None` to follow the system default) in `config.json`
    /// and in the state the next recording reads, and answers with what it stored.
    pub fn set_input(&self, uid: Option<&str>) -> Result<InputChoice, VoiceError> {
        let uid = uid.map(str::trim);
        if uid == Some("") {
            return Err(VoiceError::Failed(EMPTY_INPUT.to_string()));
        }
        let mut stored = self.stored.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
        let input = uid.map(|uid| StoredInput {
            // The name is kept for display only (S4): what the device calls itself now, else the
            // name already stored for it, else the UID, so a device that is gone still reads as
            // something in the panel.
            name: (self.devices)()
                .0
                .into_iter()
                .find(|device| device.uid == uid)
                .map(|device| device.name)
                .or_else(|| stored.as_ref().filter(|input| input.uid == uid).map(|input| input.name.clone()))
                .unwrap_or_else(|| uid.to_string()),
            uid: uid.to_string(),
        });
        config::save_voice_input(&self.config, input.as_ref()).map_err(VoiceError::Failed)?;
        let chosen = input.as_ref().map(|input| input.uid.clone());
        *stored = input;
        // The detector listens on the same microphone the notes record from (S2).
        if let Some(wake) = &self.wake {
            wake.set_device(chosen.clone());
        }
        Ok(InputChoice { chosen })
    }

    pub fn status(&self) -> VoiceStatus {
        self.voice().map_or_else(|_| VoiceStatus::unavailable(UNSUPPORTED), Voice::status)
    }

    /// Starting a recording drops the wake word's listener (one capture at a time, S2); once the
    /// recording is over it waits for the phrase again, unless the user turned it off.
    pub fn start(&self, locale: Option<&str>, events: Events) -> Result<(), VoiceError> {
        // The recording reads the choice stored in Rust state, so the panel never passes a device.
        let chosen = self.chosen();
        let started = self.voice()?.start(locale, chosen.as_deref(), events);
        self.listen_again();
        started
    }

    pub fn stop(&self) -> Result<Transcript, VoiceError> {
        let stopped = self.voice()?.stop();
        if let Some(wake) = &self.wake {
            if wake.enabled() {
                wake.note_ended_by_panel();
            }
        }
        self.listen_again();
        self.release_if_idle();
        stopped
    }

    pub fn cancel(&self) -> Result<(), VoiceError> {
        let cancelled = self.voice()?.cancel();
        if let Some(wake) = &self.wake {
            if wake.enabled() {
                wake.note_ended_by_panel();
            }
        }
        self.listen_again();
        self.release_if_idle();
        cancelled
    }

    fn listen_again(&self) {
        if let Some(wake) = &self.wake {
            wake.resume();
        }
    }

    /// Lets the one engine go when nothing is waiting on it (T9): the panel's own dictation leaves
    /// the microphone closed unless the detector re-armed itself on it, so no capture is left open
    /// with nothing to deliver to.
    fn release_if_idle(&self) {
        if self.wake.as_ref().is_none_or(|wake| !wake.enabled()) {
            if let Some(voice) = &self.voice {
                voice.shutdown();
            }
        }
    }

    /// `{enabled, available, reason?}`.
    pub fn wake_status(&self) -> WakeStatus {
        self.wake.as_ref().map_or_else(|| WakeStatus::unavailable(UNSUPPORTED), Wake::status)
    }

    /// Turns the detector on or off now and saves the flag (S1). On without a reference is
    /// refused with the reason: never silently. Off with no backend at all is still saved, so
    /// the panel's switch keeps its position here.
    pub fn wake_set(&self, enabled: bool) -> Result<WakeStatus, String> {
        self.wake.as_ref().ok_or_else(|| UNSUPPORTED.to_string())?.set(enabled)
    }

    /// Changes the phrase without touching the switch (S30.7).
    pub fn wake_phrase_set(&self, phrase: &str) -> Result<WakeStatus, String> {
        self.wake.as_ref().ok_or_else(|| UNSUPPORTED.to_string())?.set_phrase(phrase)
    }
}

/// The `wake_word_status` reply: `{enabled, available, reason?}`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WakeStatus {
    pub enabled: bool,
    pub available: bool,
    /// The phrase in words, as the panel typed it.
    pub phrase: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

impl WakeStatus {
    fn unavailable(reason: impl Into<String>) -> Self {
        WakeStatus {
            enabled: false,
            available: false,
            reason: Some(reason.into()),
            phrase: crate::config::DEFAULT_WAKE_PHRASE.to_string(),
        }
    }
}

/// A detector a test scripts in place of the local model. Production injects none, so the
/// local model is what listens and the saved phrase is the only thing that can start a note.
type Load = Arc<dyn Fn(&Path) -> Result<Arc<dyn WakeWord>, String> + Send + Sync>;

/// The wake word (S30.7, S1-S7): the saved flag, the reference it needs, and the log every match
/// and every failure is appended to (S5).
///
/// A detection is handled off the audio thread (the machine spawns one), starts the recording the
/// panel's button starts (S3) and ends it by energy (S6); every transcript and event reaches the
/// panel window, which sends it the way it sends one it stopped itself.
#[derive(Clone)]
pub struct Wake {
    inner: Arc<WakeInner>,
    /// A detector a test scripts; production leaves it empty so the local model listens.
    detector: Option<Load>,
    /// The pip that tells the speaker the note started (S30.7).
    chime: Arc<dyn Chime>,
}

struct WakeInner {
    voice: Option<Voice>,
    /// `<data dir>/config.json`, where the flag lives (S1).
    config_file: PathBuf,
    /// Where the local model is installed, when it is: the model manager's own answer, over the
    /// models root `model.rs` uses, so the spike never guesses a path.
    parakeet: Locate,
    /// `<data dir>/wake-word.log` (S5).
    log: PathBuf,
    events: Events,
    enabled: Mutex<bool>,
    /// The microphone the panel chose (`None` is the system default), so the detector and the
    /// note it starts listen to the same one (S2).
    device: Mutex<Option<String>>,
    /// Bumped whenever the switch changes, so a retry scheduled for an older state stops.
    generation: AtomicU64,
    /// The deadline's own clock, in milliseconds (S4).
    deadline_ms: std::sync::atomic::AtomicU64,
    /// Counts the notes the detector started, so a deadline only ends the note it was started for.
    notes: AtomicU64,
    /// The listen channel's own clock: how often it is checked, and how long a capture may stay
    /// open without a frame before it counts as lost (S1).
    guard_tick_ms: std::sync::atomic::AtomicU64,
    guard_silent_ms: std::sync::atomic::AtomicU64,
    /// How many recoveries in a row the current capture needed; a single frame resets it.
    silent_attempts: AtomicUsize,
    /// Set when the chosen input never delivers and the system default is used instead, for this
    /// session only: the stored choice is never overwritten (S1).
    force_default: AtomicBool,
    /// Set when the built-in microphone had to take over because nothing else delivered audio.
    use_built_in: AtomicBool,
    /// When the capture currently armed opened, and how many frames the app had seen then: the
    /// supervisor's only measure of "this capture is alive".
    armed_ms: AtomicU64,
    frames_at_arm: AtomicU64,
    /// The same for the note that is running, when one is.
    note_ms: AtomicU64,
    frames_at_note: AtomicU64,
    /// One supervisor per app, not one per arm: the microphone has a single owner.
    supervising: AtomicBool,
    /// Until when the supervisor leaves the device alone after every input failed: cycling it is
    /// worse than waiting for the next note or toggle.
    hold_until_ms: AtomicU64,
    /// What the detector listens for; the panel may change it (S30.7).
    phrase: Mutex<wake::Phrase>,
    /// Why the last attempt to listen failed, cleared when one succeeds: the panel shows it, so
    /// the switch cannot claim to be listening while the microphone is gone (S1).
    failure: Mutex<Option<String>>,
}

impl Wake {
    pub fn new(voice: Option<Voice>, config: &DesktopConfig, events: Events, parakeet: Locate) -> Self {
        let wake = Wake {
            inner: Arc::new(WakeInner {
                voice,
                config_file: config.data_dir.join("config.json"),
                parakeet,
                log: config.data_dir.join("wake-word.log"),
                events,
                enabled: Mutex::new(false),
                device: Mutex::new(config.voice_input.as_ref().map(|input| input.uid.clone())),
                guard_tick_ms: std::sync::atomic::AtomicU64::new(CAPTURE_TICK_MS),
                guard_silent_ms: std::sync::atomic::AtomicU64::new(CAPTURE_SILENT_MS),
                silent_attempts: AtomicUsize::new(0),
                force_default: std::sync::atomic::AtomicBool::new(false),
                use_built_in: AtomicBool::new(false),
                armed_ms: AtomicU64::new(0),
                frames_at_arm: AtomicU64::new(0),
                note_ms: AtomicU64::new(0),
                frames_at_note: AtomicU64::new(0),
                supervising: AtomicBool::new(false),
                hold_until_ms: AtomicU64::new(0),
                deadline_ms: std::sync::atomic::AtomicU64::new(NOTE_DEADLINE_MS),
                notes: AtomicU64::new(0),
                phrase: Mutex::new(wake::Phrase::typed(&config.wake_phrase)),
                generation: AtomicU64::new(0),
                failure: Mutex::new(None),
            }),
            detector: None,
            chime: wake::platform_chime(),
        };
        wake
    }

    /// A scripted pip, so the rule that it fires once per note is tested on every platform.
    #[cfg(test)]
    fn with_chime(mut self, chime: Arc<dyn Chime>) -> Self {
        self.chime = chime;
        self
    }

    /// A scripted detector, so the rules around it are tested on every platform.
    #[cfg(test)]
    fn with_detector(mut self, load: Load) -> Self {
        self.detector = Some(load);
        self
    }

    /// `{enabled, available, reason?}`: `reason` is what stops it listening here, which is the
    /// panel's tooltip when the switch cannot be used (S1).
    pub fn status(&self) -> WakeStatus {
        let enabled = self.enabled();
        match self.reason().or_else(|| enabled.then(|| self.failure()).flatten()) {
            None => WakeStatus { enabled, available: true, reason: None, phrase: self.phrase().text },
            Some(reason) => WakeStatus { enabled, available: false, reason: Some(reason), phrase: self.phrase().text },
        }
    }

    /// The phrase the detector is listening for.
    fn phrase(&self) -> wake::Phrase {
        self.inner.phrase.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// Stores a new phrase, applies it now and saves it. The detector is rebuilt around it, so
    /// the change is heard on the next window instead of after a restart (S30.7).
    pub fn set_phrase(&self, typed: &str) -> Result<WakeStatus, String> {
        let phrase = wake::Phrase::typed(&crate::config::valid_wake_phrase(typed)?);
        *self.inner.phrase.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = phrase;
        crate::config::save_wake_phrase(&self.inner.config_file, crate::config::valid_wake_phrase(typed)?.as_str())?;
        if self.enabled() {
            self.disarm();
            self.resume();
        }
        self.announce();
        Ok(self.status())
    }

    fn generation(&self) -> u64 {
        self.inner.generation.load(Ordering::SeqCst)
    }

    fn failure(&self) -> Option<String> {
        self.inner.failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    fn set_failure(&self, reason: Option<String>) {
        *self.inner.failure.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = reason;
    }

    /// Tries to listen again after a while, because a microphone that refuses to start is usually
    /// gone for a moment (a headset unplugged, the Mac waking, another app holding the input), and
    /// giving up left the switch on with nothing listening (S1).
    fn retry(&self, attempt: usize) {
        // The ladder keeps going at its slowest step for as long as the detector is on: a lost
        // resume must never leave the wake word dead until the switch is toggled (S1).
        let delegate = self.clone();
        let generation = self.generation();
        let wait = std::time::Duration::from_millis(RETRY_MS[attempt.min(RETRY_MS.len() - 1)]);
        let spawned = std::thread::Builder::new().name("wake-retry".into()).spawn(move || {
            std::thread::sleep(wait);
            if delegate.generation() != generation || !delegate.enabled() {
                return;
            }
            delegate.resume_after(attempt + 1);
        });
        if spawned.is_err() {
            self.log("failed", serde_json::json!({ "reason": "cannot schedule a retry for the microphone" }));
        }
    }

    /// The microphone the panel chose, or `None` for the system default.
    fn device(&self) -> Option<String> {
        #[cfg(target_os = "macos")]
        if self.inner.use_built_in.load(Ordering::SeqCst) {
            // Neither the choice nor the default delivered a frame: the built-in keeps the channel.
            return Some(BUILT_IN_INPUT.to_string());
        }
        if self.inner.force_default.load(Ordering::SeqCst) {
            // The chosen input never delivered a frame; the default keeps the channel alive.
            return None;
        }
        self.inner.device.lock().unwrap_or_else(|poisoned| poisoned.into_inner()).clone()
    }

    /// True the first time the built-in takes over, so the log says it once.
    fn fall_back_to_built_in(&self, because: &str) -> bool {
        if self.inner.use_built_in.swap(true, Ordering::SeqCst) {
            return false;
        }
        self.log("device_fallback", serde_json::json!({ "to": "the built-in microphone", "because": because }));
        true
    }

    /// Keeps the detector on the same microphone the panel just chose.
    fn set_device(&self, device: Option<String>) {
        *self.inner.device.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = device;
    }

    fn enabled(&self) -> bool {
        *self.inner.enabled.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_enabled(&self, enabled: bool) {
        *self.inner.enabled.lock().unwrap_or_else(|poisoned| poisoned.into_inner()) = enabled;
        // Any retry scheduled for the previous state is stale now.
        self.inner.generation.fetch_add(1, Ordering::SeqCst);
    }

    /// Why nothing can listen here: no detector on this platform, or no reference file (S4).
    fn reason(&self) -> Option<String> {
        // Listening needs the microphone tap this app has on macOS and Linux, and a detector:
        // the local model in production, a scripted one in a test.
        let platform = cfg!(any(target_os = "macos", target_os = "linux"));
        if self.inner.voice.is_none() || !platform {
            return Some(UNSUPPORTED.to_string());
        }
        // A scripted detector (tests only) answers for itself; otherwise the local model is the
        // only detector, so without it there is nothing to listen with.
        if self.detector.is_some() {
            return None;
        }
        match self.local() {
            Some(_) => None,
            None => Some(wake::parakeet_absent()),
        }
    }

    /// The installed local model's directory, when it is installed (`model.rs` checks every file
    /// it needs).
    fn local(&self) -> Option<PathBuf> {
        (self.inner.parakeet)()
    }

    /// Turns the detector on or off, applying it now and saving the flag in `config.json` (S1).
    /// On without the reference fails with the reason before anything is written.
    pub fn set(&self, enabled: bool) -> Result<WakeStatus, String> {
        if enabled {
            if let Some(reason) = self.reason() {
                return Err(reason);
            }
            let voice = self.inner.voice.clone().ok_or_else(|| UNSUPPORTED.to_string())?;
            // The switch says on before the channel opens: the supervisor that watches that channel
            // starts with the arm and must not see the previous state.
            self.set_enabled(true);
            // A person asking again is a fresh start: the chain begins at the chosen device.
            self.inner.silent_attempts.store(0, Ordering::SeqCst);
            self.inner.force_default.store(false, Ordering::SeqCst);
            self.inner.use_built_in.store(false, Ordering::SeqCst);
            self.inner.hold_until_ms.store(0, Ordering::SeqCst);
            if let Err(error) = self.listen(&voice) {
                self.set_enabled(false);
                return Err(error.to_string());
            }
        } else {
            self.set_enabled(false);
            // The engine the whole app shares is closed here and nowhere else: a note no longer
            // opens or closes the device (T9).
            self.shutdown();
        }
        if let Err(error) = crate::config::save_wake_word(&self.inner.config_file, enabled) {
            // The flag lives in memory and on disk; when the write fails the two disagree unless one
            // is undone. The microphone keeps the state the switch shows (T7).
            self.log("failed", serde_json::json!({ "step": "save", "enabled": enabled, "reason": error }));
            if enabled {
                self.set_enabled(false);
                self.shutdown();
            }
            return Err(error);
        }
        Ok(self.status())
    }

    /// Applies the flag read at startup (S1), logging why it cannot listen instead of doing
    /// nothing (S5).
    pub fn startup(&self, enabled: bool) {
        if !enabled {
            return;
        }
        match self.reason() {
            Some(reason) => self.log("failed", serde_json::json!({ "reason": reason })),
            None => {
                self.set_enabled(true);
                self.resume();
            }
        }
    }

    /// Waits for the phrase on the microphone the app already has (S2): a fresh detector, so the
    /// audio of the last note cannot match the next one.
    fn listen(&self, voice: &Voice) -> Result<(), VoiceError> {
        // Spike: the local model listening for the phrase when it is installed, the trained
        // reference otherwise. Loading either one takes seconds, so this runs off the audio thread.
        let detector = match &self.detector {
            Some(load) => load(self.local().as_deref().unwrap_or(Path::new(""))).map_err(VoiceError::Failed)?,
            None => {
                let dir = self.local().ok_or(VoiceError::Unsupported)?;
                Arc::new(
                    wake::ParakeetWake::load(&dir, self.inner.log.clone(), self.phrase()).map_err(VoiceError::Failed)?,
                ) as Arc<dyn WakeWord>
            }
        };
        // Diagnostic wrapper (temporary): the first frame the detector receives goes into the wake
        // log, so a run with no match can tell "no audio arrived" apart from "audio that never
        // scored". `eprintln!` cannot: a bundle launched from the Finder has no visible stderr.
        let armed = voice.arm(self.device().as_deref(), detector.clone(), self.listen_events());
        let armed = match &armed {
            Ok(()) => armed,
            Err(error) => {
                self.log("failed", serde_json::json!({ "step": "arm", "reason": error.to_string() }));
                // A device that is gone makes the arm fail outright; the built-in takes over so the
                // switch cannot end up on with nothing listening.
                let device_problem = !matches!(error, VoiceError::Busy | VoiceError::Cancelled);
                if device_problem && self.fall_back_to_built_in(&error.to_string()) {
                    voice.arm(self.device().as_deref(), detector, self.listen_events())
                } else {
                    armed
                }
            }
        };
        if armed.is_ok() {
            // The capture's own clock, and the frame count it has to beat (S1).
            self.inner.armed_ms.store(now_ms(), Ordering::SeqCst);
            self.inner.frames_at_arm.store(voice.frames_seen(), Ordering::SeqCst);
            self.supervise();
        }
        armed
    }

    /// Takes the detector off the capture without closing it (T9); the app's own recovery path, so
    /// a lost device is what needs a new capture, not a finished note.
    fn disarm(&self) {
        if let Some(voice) = &self.inner.voice {
            voice.disarm();
        }
    }

    /// Closes the machine's one capture: the switch went off, or every input failed, so nothing
    /// needs the microphone (T9).
    fn shutdown(&self) {
        if let Some(voice) = &self.inner.voice {
            voice.shutdown();
        }
    }

    /// Waits for the phrase again after a recording, which always disarms the listener.
    fn resume(&self) {
        self.resume_after(0);
    }

    /// One attempt to listen, and the retry that follows a microphone that is not ready yet.
    fn resume_after(&self, attempt: usize) {
        let Some(voice) = self.inner.voice.clone() else { return };
        // Every reason not to listen gets a line: a cycle that ends in silence is how "the
        // microphone turned itself off" stayed invisible (S6).
        if !self.enabled() {
            if !crate::config::stored_wake_word(&self.inner.config_file) {
                self.log("resume_skipped", serde_json::json!({ "because": "disabled" }));
                return;
            }
            // The switch was on in `config.json` but off here: a save that failed, or a file edited
            // by hand. The stored value wins and the detector comes back (S1).
            self.log("flag_drift", serde_json::json!({ "stored": true }));
            self.set_enabled(true);
        }
        if let Some(reason) = self.reason() {
            self.log("resume_skipped", serde_json::json!({ "because": reason }));
            return;
        }
        match self.listen(&voice) {
            // Listening again: whatever failed before no longer does, and the log says so (S6).
            Ok(()) => {
                self.set_failure(None);
                self.log("resumed", serde_json::json!({ "listening": voice.listening() }));
            }
            // The microphone is still closing the last capture, or the note has not finished
            // releasing it. Whoever was supposed to re-arm may already have run, so this retries
            // instead of assuming another path will (S1). Logged for the first steps only, so a
            // detector that stays busy for hours does not fill the log.
            Err(error @ (VoiceError::Busy | VoiceError::Cancelled)) => {
                if attempt < RETRY_MS.len() {
                    self.log(
                        "resume_failed",
                        serde_json::json!({ "reason": error.to_string(), "attempt": attempt, "retry": true }),
                    );
                }
                self.retry(attempt);
            }
            Err(error) => {
                let reason = error.to_string();
                self.set_failure(Some(reason.clone()));
                self.log("failed", serde_json::json!({ "reason": reason, "attempt": attempt }));
                self.retry(attempt);
            }
        }
    }

    /// The machine fires these off the audio thread; nothing here may run on it either, because
    /// starting a recording can prompt for a permission and stopping waits for the transcript.
    fn handle(&self, event: ListenEvent) {
        match event {
            ListenEvent::Detected(detection) => {
                // The variant matters: "oye dottie" and "oye doti" are the same command heard
                // differently, and the log is how the matcher gets tuned (S5).
                self.log(
                    "detected",
                    serde_json::json!({
                        "score": detection.score,
                        "model": detection.name,
                        "variant": detection.variant,
                    }),
                );
                if let Err(error) = self.record() {
                    self.log("failed", serde_json::json!({ "reason": error }));
                }
            }
            ListenEvent::Silent => self.stop(),
        }
    }

    /// Starts the note through the path the panel's button uses (S3): the same `Voice::start`,
    /// the same `voice://*` events, plus the automatic stop (S6).
    fn record(&self) -> Result<(), String> {
        let voice = self.inner.voice.clone().ok_or_else(|| UNSUPPORTED.to_string())?;
        // The detector gives the microphone up before the note takes it (S2).
        self.log("paused", serde_json::json!({}));
        let started = voice.start_woken(None, self.device().as_deref(), self.panel_events(), self.listen_events());
        if let Err(error) = started {
            // A note that never started must hand the detector back: the machine is already idle
            // again, so without this the wake word stays dead until the switch is toggled (S1).
            self.log("note_failed", serde_json::json!({ "reason": error.to_string() }));
            self.resume();
            return Err(error.to_string());
        }
        // The panel did not press anything, so a short system pip says the note is running (S3),
        // and it is the only sound: the automatic stop is silent.
        self.chime.note_started();
        // The panel did not press anything, so it needs to know a note is running (S3).
        (self.inner.events)(VoiceEvent::Started);
        self.log("note_started", serde_json::json!({}));
        // The supervisor owns the note's timeouts now: it is the one watching the microphone (S4).
        self.inner.note_ms.store(now_ms(), Ordering::SeqCst);
        self.inner.frames_at_note.store(voice.frames_seen(), Ordering::SeqCst);
        self.inner.notes.fetch_add(1, Ordering::SeqCst);
        self.announce();
        Ok(())
    }

    /// Ends a note the detector started: the transcript reaches the panel in the same shape as
    /// the `voice_stop` reply, so the panel sends it the way it sends one it stopped (S3).
    fn stop(&self) {
        if let Some(voice) = &self.inner.voice {
            match voice.stop() {
                // Always, empty included: the panel shows a note nobody started, so it has no reply
                // to wait for and stays in "recording" forever if this event never arrives. An empty
                // transcript is how a silent capture ends, and the panel already knows what to do
                // with it.
                Ok(transcript) => (self.inner.events)(VoiceEvent::Transcript(transcript.text)),
                Err(error) => {
                    let reason = error.to_string();
                    self.log("failed", serde_json::json!({ "reason": reason }));
                    // A note that could not be stopped still has to leave the panel's recording
                    // state, or the same freeze follows.
                    (self.inner.events)(VoiceEvent::Transcript(String::new()));
                }
            }
        }
        // The note's audio stops here; the capture the machine owns stays open for the detector
        // that takes it back next (T9, S4).
        self.log("note_stopped", serde_json::json!({}));
        self.resume();
        self.announce();
    }

    /// Publishes the one truth the panel mirrors: whether the detector is on, what it is listening
    /// for, and what the capture is doing. Called on every transition that changes any of them (T7).
    fn announce(&self) {
        let status = self.status();
        let phase = match &self.inner.voice {
            Some(voice) if voice.busy() => "recording",
            Some(voice) if voice.listening() => "listening",
            _ => "idle",
        };
        let state = serde_json::json!({
            "wake": status,
            "capture": { "phase": phase, "device": self.device() },
        });
        (self.inner.events)(VoiceEvent::State(state));
    }

    /// The microphone's only owner: one thread for the app's whole life decides whether the channel
    /// is listening, recovering, or watching a note. Every other actor used to open and close the
    /// device on its own, and their orders contradicted (T5).
    fn supervise(&self) {
        if self.inner.supervising.swap(true, Ordering::SeqCst) {
            return;
        }
        let delegate = self.clone();
        let spawned = std::thread::Builder::new().name("wake-supervisor".into()).spawn(move || loop {
            let tick = delegate.inner.guard_tick_ms.load(Ordering::SeqCst);
            std::thread::sleep(std::time::Duration::from_millis(tick.max(20)));
            if !delegate.enabled() {
                continue;
            }
            let Some(voice) = delegate.inner.voice.clone() else { continue };
            if voice.recording() || voice.starting() {
                delegate.watch_note(&voice);
            } else if voice.listening() {
                delegate.watch_capture(&voice);
            } else if now_ms() < delegate.inner.hold_until_ms.load(Ordering::SeqCst) {
                // Every input failed a moment ago; retrying now would only cycle the device.
                continue;
            } else {
                // Nothing owns the microphone and nothing is listening: this is the one place that
                // brings the channel back, whatever the previous actor left behind (S1).
                delegate.log("channel_recovered", serde_json::json!({ "because": "nothing was open" }));
                delegate.recover();
            }
        });
        if spawned.is_err() {
            self.inner.supervising.store(false, Ordering::SeqCst);
            self.log("failed", serde_json::json!({ "reason": "the supervisor could not start" }));
        }
    }

    /// A note is running: the supervisor owns its stall, its deadline and its deafness, instead of
    /// three separate timers racing (S4).
    fn watch_note(&self, voice: &Voice) {
        let started = self.inner.note_ms.load(Ordering::SeqCst);
        let elapsed = now_ms().saturating_sub(started);
        if voice.starting() && elapsed > STALL_MS {
            self.log("note_stalled", serde_json::json!({ "after_ms": elapsed }));
            self.log("note_stopped", serde_json::json!({ "from": "stalled" }));
            if let Err(error) = voice.cancel() {
                self.log("failed", serde_json::json!({ "reason": error.to_string() }));
            }
            self.recover();
            return;
        }
        // A capture that opens and never delivers a frame never reaches the auto-stop, which counts
        // the audio it hears: the supervisor sees the engine's frame counter itself (S4, T9).
        let silent = voice.frames_seen() == self.inner.frames_at_note.load(Ordering::SeqCst);
        if silent && elapsed > self.inner.guard_silent_ms.load(Ordering::SeqCst) {
            self.log("note_deaf", serde_json::json!({ "after_ms": elapsed }));
            self.stop();
            return;
        }
        let deadline = self.inner.deadline_ms.load(Ordering::SeqCst);
        if elapsed > deadline {
            self.log("note_deadline", serde_json::json!({ "after_ms": elapsed }));
            self.stop();
        }
    }

    /// A capture is listening: it counts as alive only when the engine's own buffer count moved,
    /// whoever the sink was. Otherwise the chain walks to the next input — the same device, then the
    /// default, then the built-in — and the supervisor is the only caller of either side (S1, T8).
    fn watch_capture(&self, voice: &Voice) {
        let frames = voice.frames_seen();
        if frames > self.inner.frames_at_arm.load(Ordering::SeqCst) {
            // Frames arrive, so this capture is healthy and the next loss starts the chain over.
            self.inner.silent_attempts.store(0, Ordering::SeqCst);
            self.inner.force_default.store(false, Ordering::SeqCst);
            self.inner.use_built_in.store(false, Ordering::SeqCst);
            return;
        }
        let age = now_ms().saturating_sub(self.inner.armed_ms.load(Ordering::SeqCst));
        if age < self.inner.guard_silent_ms.load(Ordering::SeqCst) {
            // A capture gets its whole tolerance to deliver a first frame.
            return;
        }
        let attempt = self.inner.silent_attempts.fetch_add(1, Ordering::SeqCst) + 1;
        self.log("capture_lost", serde_json::json!({ "silent_ms": age, "attempt": attempt, "device": self.device() }));
        if attempt > CAPTURE_RETRIES {
            if !self.inner.force_default.swap(true, Ordering::SeqCst) {
                self.log("device_fallback", serde_json::json!({ "to": "the system default input", "attempt": attempt }));
            } else if !self.inner.use_built_in.swap(true, Ordering::SeqCst) {
                self.log("device_fallback", serde_json::json!({ "to": "the built-in microphone", "attempt": attempt }));
            } else {
                // Every input had its turn. Close the capture instead of leaving it open and mute,
                // and hold off: the next note, toggle or restart starts a fresh chain.
                self.log("capture_gave_up", serde_json::json!({ "attempts": attempt }));
                self.inner.hold_until_ms.store(now_ms() + CAPTURE_HOLD_MS, Ordering::SeqCst);
                // Closed, not merely disarmed: an engine that delivers nothing is exactly the
                // "microphone on, deaf" state the user saw (T9).
                self.shutdown();
                return;
            }
        }
        self.recover();
    }

    /// The machine under this detector, for a test that needs to press its buttons (T5).
    #[cfg(test)]
    pub fn voice(&self) -> Option<Voice> {
        self.inner.voice.clone()
    }

    /// The supervisor's clock and tolerance, shortened so a test does not wait seconds (S1).
    pub fn set_capture_guard(&self, tick: std::time::Duration, tolerance: std::time::Duration) {
        self.inner.guard_tick_ms.store(tick.as_millis() as u64, Ordering::SeqCst);
        self.inner.guard_silent_ms.store(tolerance.as_millis() as u64, Ordering::SeqCst);
    }

    /// Gives the microphone back to a fresh capture, from the one place allowed to do it.
    fn recover(&self) {
        self.disarm();
        self.resume();
    }

    /// The deadline, shortened so a test does not wait a minute for it (S4).
    pub fn set_note_deadline(&self, deadline: std::time::Duration) {
        self.inner.deadline_ms.store(deadline.as_millis() as u64, Ordering::SeqCst);
    }

    /// A note the panel ended, so the cycle has a line for it too (S6).
    pub fn note_ended_by_panel(&self) {
        self.log("note_stopped", serde_json::json!({ "from": "panel" }));
    }

    fn listen_events(&self) -> ListenEvents {
        let wake = self.clone();
        Arc::new(move |event| wake.handle(event))
    }

    fn panel_events(&self) -> Events {
        let events = self.inner.events.clone();
        Arc::new(move |event| events(event))
    }

    /// One JSON line per detection (with its score) and per failure (S5), so real use collects the
    /// data the operating point still lacks. Opened, appended and flushed per line: no dependency,
    /// and a log that cannot be written never breaks listening.
    fn log(&self, event: &str, details: serde_json::Value) {
        let mut entry = serde_json::json!({ "at": now_ms(), "event": event });
        if let (Some(fields), serde_json::Value::Object(more)) = (entry.as_object_mut(), details) {
            fields.extend(more);
        }
        let mut line = entry.to_string();
        line.push('\n');
        let written = std::fs::OpenOptions::new().create(true).append(true).open(&self.inner.log).and_then(|mut file| {
            use std::io::Write;
            file.write_all(line.as_bytes())?;
            file.flush()
        });
        if let Err(error) = written {
            eprintln!("gentle-dot: cannot write {}: {error}", self.inner.log.display());
        }
    }
}


/// Milliseconds since the epoch, for the log's timestamp.
fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or(0)
}

/// The local model's files and the recognizer that keeps it loaded.
pub struct VoiceModel {
    manager: Arc<ModelManager>,
    local: Arc<Parakeet>,
}

impl VoiceModel {
    pub fn status(&self) -> ModelStatus {
        self.manager.status()
    }

    pub fn download(&self) -> Result<(), ModelError> {
        self.manager.download()
    }

    pub fn cancel(&self) {
        self.manager.cancel();
    }

    /// Deletes the files and drops the loaded model.
    pub fn remove(&self) -> Result<(), ModelError> {
        self.manager.remove()?;
        self.local.release();
        Ok(())
    }
}

/// Where models are installed: `<app data>/models`.
pub fn models_dir(app_data: Option<PathBuf>) -> PathBuf {
    app_data.unwrap_or_else(|| std::env::temp_dir().join("gentle-dot")).join("models")
}

/// Voice input with the engine this platform prefers, and the local model's manager.
pub fn start(app: &AppHandle) -> (VoiceInput, VoiceModel) {
    let emitter = app.clone();
    let events: ModelEvents = Arc::new(move |event: ModelEvent| {
        if let Err(error) = emitter.emit_to(EventTarget::webview_window(crate::shell::PANEL), MODEL_EVENT, event) {
            eprintln!("gentle-dot: cannot send a voice model event: {error}");
        }
    });
    let manager = Arc::new(ModelManager::new(
        ModelSpec::parakeet(),
        models_dir(app.path().app_data_dir().ok()),
        Arc::new(HttpFetcher),
        Arc::new(SystemClock),
        events,
        fallback_engine(),
    ));
    let installed = manager.clone();
    let locate: Locate = Arc::new(move || installed.installed());
    let loader: Loader = Arc::new(parakeet::load);
    let local = Arc::new(Parakeet::new(locate.clone(), loader));
    let mut input = platform_input(local.clone());
    let config = crate::config::load_config();
    let panel = app.clone();
    let target = EventTarget::webview_window(crate::shell::PANEL);
    let panel_events: Events = Arc::new(move |event: VoiceEvent| {
        if let Err(error) = panel.emit_to(target.clone(), event.name(), event.payload()) {
            eprintln!("gentle-dot: cannot send a voice event: {error}");
        }
    });
    let wake = Wake::new(input.voice.clone(), &config, panel_events, locate);
    // S1: the flag is read here and applied now; a failure is logged with its reason.
    wake.startup(config.wake_word);
    input.wake = Some(wake);
    // S1, S2: the panel's list follows the system from here on, instead of the snapshot it read
    // when it opened. A listener that cannot be installed says so instead of going quiet (S4).
    let devices = app.clone();
    let devices_target = EventTarget::webview_window(crate::shell::PANEL);
    let device_events: DevicesEvents = Arc::new(move |options: &InputOptions| {
        if let Err(error) = devices.emit_to(devices_target.clone(), super::DEVICES_EVENT, options.clone()) {
            eprintln!("gentle-dot: cannot send the microphone list: {error}");
        }
    });
    input.watch_devices(device_events);
    (input, VoiceModel { manager, local })
}

/// macOS: the native microphone, Parakeet when installed, else Apple Speech. Linux: a
/// command-line recorder and Parakeet. Elsewhere: nothing.
fn platform_input(local: Arc<Parakeet>) -> VoiceInput {
    #[cfg(target_os = "macos")]
    return VoiceInput::new(Voice::new(
        Arc::new(super::MacMicrophone),
        Arc::new(Preferred::new(local, Some(Arc::new(super::AppleSpeech)))),
        Arc::new(SystemClock),
    ));
    #[cfg(target_os = "linux")]
    return VoiceInput::new(Voice::new(
        Arc::new(super::pipe::PipeMicrophone::system()),
        Arc::new(Preferred::new(local, None)),
        Arc::new(SystemClock),
    ));
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        let _ = local;
        VoiceInput::unsupported()
    }
}

async fn blocking<T: Send + 'static>(
    app: AppHandle,
    action: impl FnOnce(&VoiceInput) -> Result<T, VoiceError> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || action(&app.state::<VoiceInput>()))
        .await
        .map_err(|error| error.to_string())?
        .map_err(String::from)
}

#[tauri::command]
pub async fn voice_status(app: AppHandle) -> Result<VoiceStatus, String> {
    blocking(app, |input| Ok(input.status())).await
}

#[tauri::command]
pub async fn voice_start(app: AppHandle, webview: tauri::WebviewWindow, locale: Option<String>) -> Result<(), String> {
    let target = EventTarget::webview_window(webview.label());
    let emitter = app.clone();
    let events: Events = Arc::new(move |event: VoiceEvent| {
        if let Err(error) = emitter.emit_to(target.clone(), event.name(), event.payload()) {
            eprintln!("gentle-dot: cannot send a voice event: {error}");
        }
    });
    blocking(app, move |input| input.start(locale.as_deref(), events)).await
}

#[tauri::command]
pub async fn voice_stop(app: AppHandle) -> Result<Transcript, String> {
    blocking(app, VoiceInput::stop).await
}

#[tauri::command]
pub async fn voice_cancel(app: AppHandle) -> Result<(), String> {
    blocking(app, VoiceInput::cancel).await
}

/// `{enabled, available, reason?}` (S1): whether it is on, and if not, why it cannot be.
/// `{devices, chosen, default}`: the microphones the panel may choose from (S1-S4).
#[tauri::command]
pub async fn voice_input_options(app: AppHandle) -> Result<InputOptions, String> {
    blocking(app, |input| Ok(input.input_options())).await
}

/// Stores the chosen microphone (`null` follows the system default) and answers `{chosen}`.
#[tauri::command]
pub async fn voice_input_set(app: AppHandle, uid: Option<String>) -> Result<InputChoice, String> {
    blocking(app, move |input| input.set_input(uid.as_deref())).await
}

#[tauri::command]
pub async fn wake_word_status(app: AppHandle) -> Result<WakeStatus, String> {
    blocking(app, |input| Ok(input.wake_status())).await
}

/// Stores the phrase the detector listens for (S30.7): one to four words, applied on the spot.
#[tauri::command]
pub async fn wake_phrase_set(app: AppHandle, phrase: String) -> Result<WakeStatus, String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<VoiceInput>().wake_phrase_set(&phrase))
        .await
        .map_err(|error| error.to_string())?
}

/// Turns the wake word on or off (S1). On without the reference fails with the reason.
#[tauri::command]
pub async fn wake_word_set(app: AppHandle, enabled: bool) -> Result<WakeStatus, String> {
    tauri::async_runtime::spawn_blocking(move || app.state::<VoiceInput>().wake_set(enabled))
        .await
        .map_err(|error| error.to_string())?
}

async fn model<T: Send + 'static>(
    app: AppHandle,
    action: impl FnOnce(&VoiceModel) -> Result<T, ModelError> + Send + 'static,
) -> Result<T, String> {
    tauri::async_runtime::spawn_blocking(move || action(&app.state::<VoiceModel>()))
        .await
        .map_err(|error| error.to_string())?
        .map_err(String::from)
}

/// `{installed, bytes?, downloading, received?, total?, engine}`.
#[tauri::command]
pub async fn voice_model_status(app: AppHandle) -> Result<ModelStatus, String> {
    model(app, |model| Ok(model.status())).await
}

/// Starts the download; `voice://model` events report progress and the outcome.
#[tauri::command]
pub async fn voice_model_download(app: AppHandle) -> Result<(), String> {
    model(app, VoiceModel::download).await
}

/// Stops a download in flight (harmless otherwise); a `failed` event follows.
#[tauri::command]
pub async fn voice_model_cancel(app: AppHandle) -> Result<(), String> {
    model(app, |model| {
        model.cancel();
        Ok(())
    })
    .await
}

/// Deletes the model; a `removed` event follows when there was one.
#[tauri::command]
pub async fn voice_model_remove(app: AppHandle) -> Result<(), String> {
    model(app, VoiceModel::remove).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fake::{FakeClock, FakeMicrophone, FakeRecognizer, Recorder};
    use crate::voice::wake::FakeWake;
    use crate::voice::{VoiceError, VoiceEvent, VoiceStatus, UNSUPPORTED};
    use std::path::PathBuf;
    use std::sync::Arc;
    use std::sync::atomic::AtomicU64;
    use std::time::Duration;

    #[test]
    fn without_a_backend_voice_is_unavailable_and_every_action_errors() {
        let input = VoiceInput::unsupported();
        assert_eq!(input.status(), VoiceStatus::unavailable(UNSUPPORTED));
        assert_eq!(input.start(None, Arc::new(|_| {})), Err(VoiceError::Unsupported));
        assert_eq!(input.stop(), Err(VoiceError::Unsupported));
        assert_eq!(input.cancel(), Err(VoiceError::Unsupported));
        assert_eq!(String::from(VoiceError::Unsupported), "Voice input is available on macOS and Linux.");
    }

    #[test]
    fn without_a_backend_the_wake_word_is_unavailable_and_cannot_be_turned_on() {
        let input = VoiceInput::unsupported();
        assert_eq!(
            input.wake_status(),
            WakeStatus {
                enabled: false,
                available: false,
                reason: Some(UNSUPPORTED.into()),
                phrase: crate::config::DEFAULT_WAKE_PHRASE.into(),
            }
        );
        assert_eq!(input.wake_set(true), Err(UNSUPPORTED.to_string()));
    }

    #[test]
    fn the_command_names_match_the_l88_and_s30_5_contracts() {
        assert_eq!(
            COMMANDS,
            [
                "voice_status",
                "voice_start",
                "voice_stop",
                "voice_cancel",
                "voice_input_options",
                "voice_input_set",
                "voice_model_status",
                "voice_model_download",
                "voice_model_cancel",
                "voice_model_remove",
                "wake_word_status",
                "wake_word_set",
                "wake_phrase_set",
            ]
        );
    }

    #[test]
    fn models_live_under_the_app_data_dir() {
        assert_eq!(models_dir(Some(PathBuf::from("/data/app"))), PathBuf::from("/data/app/models"));
        assert_eq!(models_dir(None), std::env::temp_dir().join("gentle-dot").join("models"));
    }

    #[test]
    fn the_commands_are_registered_and_granted_to_the_panel_only() {
        let build = include_str!("../../build.rs");
        let capabilities: serde_json::Value = serde_json::from_str(include_str!("../../capabilities/default.json")).unwrap();
        let granted_to = |permission: &str| -> Vec<&str> {
            let all = capabilities["capabilities"].as_array().unwrap();
            let mut windows: Vec<&str> = all
                .iter()
                .filter(|c| c["permissions"].as_array().unwrap().iter().any(|p| p == permission))
                .flat_map(|c| c["windows"].as_array().unwrap().iter().map(|w| w.as_str().unwrap()))
                .collect();
            windows.sort();
            windows
        };
        for command in COMMANDS {
            assert!(build.contains(&format!("\"{command}\",")), "{command} is missing from build.rs");
            let permission = format!("allow-{}", command.replace('_', "-"));
            assert_eq!(granted_to(&permission), vec!["panel"], "{permission}");
        }
    }

    #[test]
    fn the_panel_listens_for_the_pushed_microphone_list() {
        // The name is the whole contract between the two sides (S1, S2): it is checked here
        // because the Rust and the TypeScript tests can never see each other.
        let ui = include_str!("../../../../../packages/ui/src/voice-native.ts");
        let name = crate::voice::DEVICES_EVENT;
        assert!(ui.contains(&format!("\"{name}\"")), "voice-native.ts does not accept {name}");
    }

    /// What the system reports to the scripted watch: a built-in microphone, and a headset once one
    /// is "connected" (S1). A `static` because `Enumerate` is a plain function pointer.
    static HEADSET_CONNECTED: AtomicBool = AtomicBool::new(false);

    fn scripted_devices() -> (Vec<StoredInput>, Option<String>) {
        let mut devices = vec![StoredInput { uid: "uid-1".into(), name: "MacBook Pro Microphone".into() }];
        if HEADSET_CONNECTED.load(Ordering::SeqCst) {
            devices.push(StoredInput { uid: "uid-3".into(), name: "AirPods Pro".into() });
        }
        (devices, Some("uid-1".into()))
    }

    #[test]
    fn the_microphone_list_follows_the_system_and_a_storm_pushes_it_once() {
        HEADSET_CONNECTED.store(false, Ordering::SeqCst);
        let mut input = VoiceInput::unsupported();
        *input.stored.lock().unwrap() = None;
        input.devices = scripted_devices;
        // The notification the system would send, held here instead of in CoreAudio (S2).
        let notified: Arc<Mutex<Option<Arc<dyn Fn() + Send + Sync>>>> = Arc::new(Mutex::new(None));
        {
            let notified = Arc::clone(&notified);
            input.watch = Arc::new(move |changed: Arc<dyn Fn() + Send + Sync>| {
                *notified.lock().unwrap() = Some(changed);
                Ok(Some(Box::new(()) as Box<dyn Send + Sync>))
            });
        }
        let pushed: Arc<Mutex<Vec<InputOptions>>> = Arc::new(Mutex::new(Vec::new()));
        let recording: DevicesEvents = {
            let pushed = Arc::clone(&pushed);
            Arc::new(move |options: &InputOptions| pushed.lock().unwrap().push(options.clone()))
        };
        input.watch_devices(recording);
        // The whole reply, pushed once as soon as the watch is armed (S1).
        assert_eq!(pushed.lock().unwrap().len(), 1);
        assert_eq!(pushed.lock().unwrap()[0], input.input_options());

        // The headset is connected and Bluetooth announces the change twice: one settled push.
        HEADSET_CONNECTED.store(true, Ordering::SeqCst);
        let notify = notified.lock().unwrap().clone().expect("the system notifies this watch");
        notify();
        notify();
        wait_until("the new list is pushed", || pushed.lock().unwrap().len() == 2);
        let settled = pushed.lock().unwrap()[1].clone();
        assert_eq!(settled.devices.len(), 2, "the headset is in the pushed list");
        assert_eq!(settled.chosen, None, "the choice travels with the list (S3)");
        assert_eq!(settled.default.as_deref(), Some("uid-1"));
        assert_eq!(settled, input.input_options(), "the push and the reply are one payload");

        // A notification that changed nothing is not pushed again: the panel must not re-render.
        notify();
        std::thread::sleep(Duration::from_millis(DEVICES_SETTLE_MS * 2));
        assert_eq!(pushed.lock().unwrap().len(), 2, "an unchanged list is not pushed again");
        // S4: the watch takes the listener and its worker with it.
        input.stop_watching_devices();
        assert!(input.watched.lock().unwrap().is_none());
    }

    fn config(dir: PathBuf) -> DesktopConfig {
        DesktopConfig {
            data_dir: dir,
            port: 0,
            shortcut: String::new(),
            voice_input: None,
            wake_word: false,
            wake_phrase: crate::config::DEFAULT_WAKE_PHRASE.into(),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("target/test-scratch/wake").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("wake-word")).unwrap();
        dir
    }

    /// Counts the pips, so the "once per note, never on the stop" rule is testable.
    #[derive(Default)]
    struct CountingChime(std::sync::atomic::AtomicUsize);

    impl Chime for CountingChime {
        fn note_started(&self) {
            self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl CountingChime {
        fn count(&self) -> usize {
            self.0.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    struct WakeRig {
        wake: Wake,
        mic: Arc<FakeMicrophone>,
        recognizer: Arc<FakeRecognizer>,
        detector: Arc<FakeWake>,
        recorder: Recorder,
        chime: Arc<CountingChime>,
        dir: PathBuf,
    }

    /// The wake word with a scripted detector and a fake microphone: the whole path, with no
    /// reference file and no audio device.
    fn wake_rig(name: &str, scripted: bool) -> WakeRig {
        let dir = scratch(name);
        let mic = Arc::new(FakeMicrophone::granted());
        let recognizer = Arc::new(FakeRecognizer::granted());
        let voice = Voice::new(mic.clone(), recognizer.clone(), Arc::new(FakeClock(AtomicU64::new(0))));
        let detector = Arc::new(FakeWake::default());
        let recorder = Recorder::default();
        let load: Load = {
            let detector = detector.clone();
            Arc::new(move |_| Ok(detector.clone() as Arc<dyn WakeWord>))
        };
        let chime = Arc::new(CountingChime::default());
        let wake = Wake::new(Some(voice), &config(dir.clone()), recorder.events(), Arc::new(|| None))
            .with_chime(chime.clone());
        let wake = if scripted { wake.with_detector(load) } else { wake };
        WakeRig { wake, mic, recognizer, detector, recorder, chime, dir }
    }

    /// Waits for a background reaction (they all run on spawned threads) before checking it.
    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !done() && std::time::Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(1));
        }
        assert!(done(), "{what}");
    }

    #[test]
    fn the_wake_word_is_off_until_it_is_turned_on_and_the_flag_is_saved() {
        let rig = wake_rig("on", true);
        assert_eq!(rig.wake.status(), WakeStatus { enabled: false, available: true, reason: None, phrase: crate::config::DEFAULT_WAKE_PHRASE.into() });
        assert_eq!(rig.wake.set(true), Ok(WakeStatus { enabled: true, available: true, reason: None, phrase: crate::config::DEFAULT_WAKE_PHRASE.into() }));
        assert_eq!(rig.mic.log.lock().unwrap().opened, 1, "listening is the only capture");
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rig.dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(saved["wake_word"], serde_json::json!(true));
        assert_eq!(rig.wake.set(false), Ok(WakeStatus { enabled: false, available: true, reason: None, phrase: crate::config::DEFAULT_WAKE_PHRASE.into() }));
        assert_eq!(rig.mic.log.lock().unwrap().closed, 1);
        let saved: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(rig.dir.join("config.json")).unwrap()).unwrap();
        assert_eq!(saved["wake_word"], serde_json::json!(false));
    }

    #[test]
    fn turning_it_on_without_the_local_model_fails_with_the_reason_and_saves_nothing() {
        // The local model is the only detector: without it nothing can hear the phrase, so the
        // switch refuses and writes nothing rather than claiming to listen.
        let rig = wake_rig("missing", false);
        let error = rig.wake.set(true).unwrap_err();
        assert_eq!(error, wake::parakeet_absent());
        assert!(!rig.wake.status().enabled);
        assert!(!rig.wake.status().available);
        assert!(!rig.dir.join("config.json").exists(), "a flag that cannot work is not saved");
        assert_eq!(rig.mic.log.lock().unwrap().opened, 0);
    }

    #[test]
    fn an_installed_local_model_listens_without_the_reference_file() {
        // The spike: with the local model installed, the trained reference is not what it needs.
        let dir = scratch("parakeet");
        let voice = Voice::new(
            Arc::new(FakeMicrophone::granted()),
            Arc::new(FakeRecognizer::granted()),
            Arc::new(FakeClock(AtomicU64::new(0))),
        );
        let locate: Locate = Arc::new(|| Some(PathBuf::from("/models/parakeet-tdt-0.6b-v3-int8")));
        let detector: Load = Arc::new(|_| Ok(Arc::new(FakeWake::default()) as Arc<dyn WakeWord>));
        let wake = Wake::new(Some(voice), &config(dir), Recorder::default().events(), locate).with_detector(detector);
        assert_eq!(wake.status(), WakeStatus { enabled: false, available: true, reason: None, phrase: crate::config::DEFAULT_WAKE_PHRASE.into() });
    }

    #[test]
    fn a_saved_flag_is_applied_at_startup() {
        let rig = wake_rig("startup", true);
        rig.wake.startup(true);
        assert!(rig.wake.status().enabled);
        assert_eq!(rig.mic.log.lock().unwrap().opened, 1);
        let quiet = wake_rig("startup-off", true);
        quiet.wake.startup(false);
        assert_eq!(quiet.mic.log.lock().unwrap().opened, 0);
    }

    #[test]
    fn a_detection_is_logged_and_starts_the_note_the_button_starts() {
        let rig = wake_rig("detected", true);
        rig.wake.set(true).unwrap();
        rig.detector.detect(0.51);
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        // The diagnostic probe logs `heard` for the first frame the detector receives, so the match
        // is found by name rather than by being the first line.
        let line = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|line| line["event"] == serde_json::json!("detected"))
            .expect("the match is logged");
        assert!((line["score"].as_f64().unwrap() - 0.51).abs() < 1e-5, "{line}");
        assert_eq!(line["model"], serde_json::json!(wake::WAKE_WORD));
        assert!(line["at"].as_u64().is_some(), "the line is timestamped: {line}");
    }

    #[test]
    fn the_log_names_the_variant_that_matched() {
        let rig = wake_rig("variant", true);
        rig.wake.set(true).unwrap();
        rig.detector.detect_variant(0.99, Some("oye dottie"));
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        let line = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .find(|line| line["event"] == serde_json::json!("detected"))
            .expect("the detection is logged");
        assert_eq!(line["variant"], serde_json::json!("oye dottie"));
        assert_eq!(line["model"], serde_json::json!(wake::WAKE_WORD));
    }

    #[test]
    fn a_note_started_by_the_wake_word_pips_once_and_the_stop_is_silent() {
        let rig = wake_rig("chime", true);
        *rig.recognizer.final_text.lock().unwrap() = Some("hola".into());
        rig.wake.set(true).unwrap();
        assert_eq!(rig.chime.count(), 0, "arming the detector is not a note");

        rig.detector.detect(0.51);
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());
        wait_until("the pip plays", || rig.chime.count() == 1);

        let sink = rig.mic.sink();
        for _ in 0..20 {
            sink.audio(&[0.05; 1_600]);
        }
        for _ in 0..16 {
            sink.audio(&[0.0; 1_600]);
        }
        wait_until("the transcript arrives", || {
            rig.recorder.seen().iter().any(|event| matches!(event, VoiceEvent::Transcript(_)))
        });
        assert_eq!(rig.chime.count(), 1, "the end of the note is silent");
    }

    #[test]
    fn a_note_that_opens_and_hears_nothing_is_ended_by_the_supervisor() {
        // The microphone's owner watches the note's own frame counter now: a note whose capture
        // opened and delivered nothing is ended and reported, instead of holding the device until
        // the deadline or forever (T5, T6).
        let rig = wake_rig("deafnote", true);
        rig.wake.set_capture_guard(Duration::from_millis(10), Duration::from_millis(60));
        rig.wake.set(true).unwrap();
        rig.detector.detect(0.6);
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());
        // The note's capture is given no audio at all.
        wait_until("the note is ended", || {
            rig.recorder.seen().iter().any(|event| matches!(event, VoiceEvent::Transcript(_)))
        });
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        assert!(log.contains("\"note_deaf\""), "{log}");
    }

    #[test]
    fn a_note_that_hears_no_speech_still_tells_the_panel_it_ended() {
        // The freeze the user hit: a note nobody pressed the button for has no reply to wait for,
        // so the panel is only told by an event. A silent note ends with an empty transcript, and
        // while that event was skipped the panel stayed in "recording" with a stopped timer and
        // dead buttons. It is always sent now.
        let rig = wake_rig("empty", true);
        rig.wake.set(true).unwrap();
        rig.detector.detect(0.6);
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());
        // Speech, then silence: the auto-stop ends it with nothing recognized.
        let sink = rig.mic.sink();
        // A second of speech first: the auto-stop only counts silence after real speech.
        for _ in 0..20 {
            sink.audio(&[0.05; 1_600]);
        }
        for _ in 0..16 {
            sink.audio(&[0.0; 1_600]);
        }
        wait_until("the panel is told the note ended", || {
            rig.recorder.seen().iter().any(|event| matches!(event, VoiceEvent::Transcript(_)))
        });
        let told = rig.recorder.seen().into_iter().find(|event| matches!(event, VoiceEvent::Transcript(_)));
        assert_eq!(told, Some(VoiceEvent::Transcript(String::new())));
    }

    #[test]
    fn the_note_ends_itself_and_the_transcript_reaches_the_panel() {
        let rig = wake_rig("transcript", true);
        *rig.recognizer.final_text.lock().unwrap() = Some(" hola ".into());
        rig.wake.set(true).unwrap();
        rig.detector.detect(0.51);
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());
        let sink = rig.mic.sink();
        for _ in 0..20 {
            sink.audio(&[0.05; 1_600]);
        }
        for _ in 0..16 {
            sink.audio(&[0.0; 1_600]);
        }
        wait_until("the transcript arrives", || {
            rig.recorder.seen().iter().any(|event| matches!(event, VoiceEvent::Transcript(_)))
        });
        let transcript = rig.recorder.seen().into_iter().find(|event| matches!(event, VoiceEvent::Transcript(_)));
        assert_eq!(transcript, Some(VoiceEvent::Transcript("hola".into())));
        // And the phrase is waited for again, on the capture the note just used (T9).
        wait_until("the detector listens again", || rig.mic.log.lock().unwrap().listening());
        assert_eq!(rig.mic.log.lock().unwrap().opened, 1, "the note did not open a capture of its own");
    }

    #[test]
    fn a_note_that_hears_nothing_ends_itself_and_gives_the_detector_back() {
        // The other half of "the microphone goes quiet": the auto-stop counts the audio the note
        // receives, so a capture that delivers none never ends the note, the microphone stays with
        // it and the detector stays paused with the switch still on. The wall clock ends it (S4).
        let rig = wake_rig("deaf", true);
        // The recognition answers at once, so the clock is the only thing this note waits on (the
        // fallback for a silent recognizer would hold `stop` for three seconds).
        *rig.recognizer.final_text.lock().unwrap() = Some(String::new());
        rig.wake.set(true).unwrap();
        // The supervisor looks often, and its deafness tolerance is wider than this test: the note is
        // ended by the wall clock, which is what is under test here (S4).
        rig.wake.set_capture_guard(Duration::from_millis(10), Duration::from_secs(60));
        rig.wake.set_note_deadline(Duration::from_millis(60));
        rig.detector.detect(0.6);
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());
        // The note is given no audio at all, and the deadline still ends it.
        wait_until("the detector listens again", || rig.mic.log.lock().unwrap().listening());
        assert_eq!(rig.mic.log.lock().unwrap().overlap, 0);
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        assert!(log.contains("\"note_deadline\""), "{log}");
        assert!(log.contains("\"note_stopped\""), "{log}");
    }

    #[test]
    fn a_capture_that_delivers_nothing_is_reopened_three_times_and_then_falls_back() {
        // "Always keep the listen channel": a capture that opened and never delivers a frame is
        // reopened with backoff, and after three recoveries the system default input takes over,
        // which is what screenpipe and Handy do around their own capture layer (S1).
        let rig = wake_rig("channel", true);
        rig.wake.set_capture_guard(Duration::from_millis(5), Duration::from_millis(20));
        // The capture's own tolerance is shortened with the supervisor's: a mute engine is replaced
        // at the next arm, so each retry really opens the device again (T9).
        rig.wake.voice().expect("the rig has a machine").set_capture_tolerance(Duration::from_millis(20));
        rig.wake.set(true).unwrap();
        // No audio is ever fed to the detector: every capture looks dead.
        std::thread::sleep(Duration::from_millis(300));
        eprintln!("LOG>>> {}", std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap_or_default());
        wait_until("the channel is recovered", || {
            let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap_or_default();
            (1..=3).all(|attempt| log.contains(&format!("\"attempt\":{attempt}")))
        });
        wait_until("the default input takes over", || {
            std::fs::read_to_string(rig.dir.join("wake-word.log")).is_ok_and(|log| log.contains("\"device_fallback\""))
        });
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        assert!(log.contains("\"capture_lost\""), "{log}");
        assert!(rig.mic.log.lock().unwrap().opened >= 4, "three retries, then the fallback");
        assert_eq!(rig.mic.log.lock().unwrap().overlap, 0, "the old engine is closed before the next opens");
        assert!(rig.wake.status().enabled, "the switch never turns itself off");
    }

    #[test]
    fn a_busy_machine_is_not_a_device_problem() {
        // The log repeated `device_fallback {"because":"Voice input is already recording"}`, which
        // walked the whole chain because the machine was closing a capture, not because the device
        // had failed. Busy and Cancelled never count as a device failure.
        let rig = wake_rig("busy", true);
        rig.wake.set(true).unwrap();
        // A second arm while the detector holds the microphone answers Busy.
        let voice = rig.wake.voice().expect("the rig has a machine");
        assert!(voice.arm(None, std::sync::Arc::new(FakeWake::default()), std::sync::Arc::new(|_| {})).is_err());
        rig.wake.resume();
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap_or_default();
        assert!(!log.contains("\"device_fallback\""), "{log}");
    }

    #[test]
    fn giving_up_closes_the_capture_and_holds() {
        // Leaving the dead capture open and mute is the "microphone on, deaf" state. When every
        // input has had its turn, the supervisor closes it and waits instead of cycling the device.
        let rig = wake_rig("hold", true);
        rig.wake.set_capture_guard(Duration::from_millis(5), Duration::from_millis(20));
        rig.wake.set(true).unwrap();
        wait_until("the supervisor gives up", || {
            std::fs::read_to_string(rig.dir.join("wake-word.log")).is_ok_and(|log| log.contains("capture_gave_up"))
        });
        wait_until("the capture is closed", || {
            let mic = rig.mic.log.lock().unwrap();
            mic.opened == mic.closed
        });
        let opened = rig.mic.log.lock().unwrap().opened;
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(rig.mic.log.lock().unwrap().opened, opened, "nothing is reopened while it holds");
    }

    #[test]
    fn a_device_that_is_gone_hands_over_to_the_built_in() {
        // Toggling the switch did nothing when the chosen input had vanished: the arm failed, the
        // switch went back off and the microphone stayed dark. The built-in microphone is the last
        // link of the chain, so the channel is still there to listen with.
        let rig = wake_rig("gone", true);
        rig.mic.set_open_error(Some(VoiceError::Failed("The microphone could not start.".into())));
        let error = rig.wake.set(true);
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap_or_default();
        assert!(log.contains("\"device_fallback\""), "{log}");
        assert!(
            rig.mic.log.lock().unwrap().devices.iter().flatten().any(|uid| uid == "BuiltInMicrophoneDevice"),
            "the built-in was asked for: {:?}",
            rig.mic.log.lock().unwrap().devices
        );
        // The fake still refuses every capture, so the switch comes back off with the reason.
        assert!(error.is_err(), "a microphone that never opens cannot claim to listen");
    }

    #[test]
    fn a_channel_that_never_delivers_stops_toggling_the_microphone() {
        // The blink the user saw: every tick reopened the capture, so the microphone lit and died
        // in a loop. Once the default input is in use and nothing arrives, the guard stops and the
        // next arm starts over instead of fighting the device forever.
        let rig = wake_rig("blink", true);
        rig.wake.set_capture_guard(Duration::from_millis(5), Duration::from_millis(20));
        rig.wake.set(true).unwrap();
        wait_until("the guard gives up", || {
            std::fs::read_to_string(rig.dir.join("wake-word.log")).is_ok_and(|log| log.contains("capture_gave_up"))
        });
        let opened = rig.mic.log.lock().unwrap().opened;
        std::thread::sleep(Duration::from_millis(250));
        assert_eq!(rig.mic.log.lock().unwrap().opened, opened, "the microphone is not toggled any more");
        assert_eq!(rig.mic.log.lock().unwrap().overlap, 0);
    }

    #[test]
    fn a_resume_that_does_not_listen_says_why() {
        // The cycle ended at note_stopped with no closing line, which is exactly how "and then it
        // never came back" stayed a mystery. A resume that declines now says so (S6).
        let rig = wake_rig("declined", true);
        rig.wake.set(true).unwrap();
        rig.wake.set(false).unwrap();
        rig.wake.resume();
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        let skipped = log
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|line| line["event"] == serde_json::json!("resume_skipped"))
            .last()
            .expect("the decline is logged");
        assert_eq!(skipped["because"], serde_json::json!("disabled"));
    }

    #[test]
    fn a_switch_the_disk_says_is_on_comes_back() {
        // The flag lives in memory and in config.json. When they disagree the switch looks on while
        // nothing listens, so the stored value wins and the detector is armed again (S1).
        let rig = wake_rig("drift", true);
        rig.wake.set(true).unwrap();
        // Memory says off, the file still says on: a save that failed, or a hand edit.
        rig.wake.set(false).unwrap();
        std::fs::write(rig.dir.join("config.json"), r#"{ "wake_word": true }"#).unwrap();
        rig.wake.resume();
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        assert!(log.contains("\"flag_drift\""), "{log}");
        assert!(rig.mic.log.lock().unwrap().opened >= 2, "the detector is listening again");
    }

    #[test]
    fn a_note_that_never_starts_gives_the_detector_back() {
        // The stranding path behind "the microphone goes quiet on its own": the note's capture never
        // opens, the machine is already idle again, so without an explicit re-arm the detector dies
        // here and stays dead (S1).
        let rig = wake_rig("orphan", true);
        rig.wake.set(true).unwrap();
        let voice = rig.wake.voice().expect("the rig has a machine");
        // The engine the note would take went away: it delivers nothing, its tolerance is short, and
        // the device refuses to open in its place (T9).
        voice.set_capture_tolerance(Duration::from_millis(20));
        rig.mic.stop_delivering();
        std::thread::sleep(Duration::from_millis(40));
        rig.mic.set_open_error(Some(VoiceError::Failed("The microphone could not start.".into())));
        rig.detector.detect(0.6);
        rig.mic.sink().audio(&[0.2; 480]);
        wait_until("the failure is logged", || {
            std::fs::read_to_string(rig.dir.join("wake-word.log")).is_ok_and(|log| log.contains("\"note_failed\""))
        });
        assert!(rig.wake.status().enabled, "the switch does not turn itself off");

        // The microphone recovers and the detector takes it back by itself.
        rig.mic.set_open_error(None);
        wait_until("the detector listens again", || rig.mic.log.lock().unwrap().listening());
        assert_eq!(rig.mic.log.lock().unwrap().overlap, 0);
        // The line is written just after the detector takes the audio back, so it is waited for.
        wait_until("the resume is logged", || {
            std::fs::read_to_string(rig.dir.join("wake-word.log")).is_ok_and(|log| log.contains("\"resumed\""))
        });
    }

    #[test]
    fn several_notes_in_a_row_keep_the_detector_listening() {
        // The bug: after a few notes the microphone went quiet and the wake word stayed dead. The
        // cycle is the fix (S2-S6) and this is the test that holds it: three notes, in a row, with
        // the detector listening again after each one. With one engine for the app's life (T9) the
        // three notes and the three arms all happen on the capture opened once, which is what makes
        // a note that opens mute impossible: 16 of 18 re-opens were mute on the machine measured.
        let rig = wake_rig("cycle", true);
        rig.recognizer.saying_each(&[" uno ", " dos ", " tres "]);
        rig.wake.set(true).unwrap();

        for round in 0..3 {
            // The detector hears the phrase on the capture it already owns.
            rig.detector.detect(0.6);
            rig.mic.sink().audio(&[0.2; 480]);
            wait_until("the note starts", || rig.mic.log.lock().unwrap().recording());

            // The note records, then ends by itself on silence.
            let note = rig.mic.sink();
            for _ in 0..20 {
                note.audio(&[0.05; 1_600]);
            }
            for _ in 0..16 {
                note.audio(&[0.0; 1_600]);
            }
            wait_until("the detector listens again", || rig.mic.log.lock().unwrap().listening());
            assert!(rig.wake.status().enabled, "the switch never turns itself off");
            let opened = rig.mic.log.lock().unwrap().opened;
            assert_eq!(opened, 1, "round {round} reopened the microphone");
        }

        // The last resume is written just after the detector takes the audio back, so the log is
        // waited for before its lines are counted.
        wait_until("every resume is logged", || {
            std::fs::read_to_string(rig.dir.join("wake-word.log"))
                .is_ok_and(|log| log.matches("\"resumed\"").count() == 3)
        });
        // One line per transition, per note (S6).
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        let lines: Vec<serde_json::Value> = log.lines().map(|line| serde_json::from_str(line).unwrap()).collect();
        for event in ["paused", "note_started", "note_stopped", "resumed"] {
            let seen = lines.iter().filter(|line| line["event"] == serde_json::json!(event)).count();
            assert_eq!(seen, 3, "{event} is logged once per note");
        }
        assert_eq!(
            lines.iter().filter(|line| line["event"] == serde_json::json!("resume_failed")).count(),
            0,
            "no resume is lost, so nothing is deferred"
        );

        // Each note carried the text it was given, in order.
        let transcripts: Vec<_> = rig
            .recorder
            .seen()
            .into_iter()
            .filter(|event| matches!(event, VoiceEvent::Transcript(_)))
            .collect();
        assert_eq!(
            transcripts,
            vec![
                VoiceEvent::Transcript("uno".into()),
                VoiceEvent::Transcript("dos".into()),
                VoiceEvent::Transcript("tres".into()),
            ]
        );

        // And the invariant: one capture, handed between the detector and the notes, never two open
        // at once (S5, T9).
        let mic = rig.mic.log.lock().unwrap();
        assert_eq!(mic.overlap, 0, "two captures were open at once: {:?}", mic.history);
        assert_eq!(mic.history.iter().filter(|step| **step == "open").count(), 1, "one engine: {:?}", mic.history);
        assert_eq!(mic.history.iter().filter(|step| **step == "close").count(), 0, "still listening");
        assert_eq!(mic.selected.iter().filter(|sink| **sink == "recording").count(), 3, "one swap per note");
        assert_eq!(mic.selected.iter().filter(|sink| **sink == "listener").count(), 4, "one per arm, and it ends armed");
    }

    #[test]
    fn a_microphone_that_refuses_to_start_is_retried_and_the_switch_says_so() {
        let rig = wake_rig("retry", true);
        rig.mic.set_open_error(Some(VoiceError::Failed("The microphone could not start.".into())));
        rig.wake.set(true).unwrap_err();
        // Turning it on failed, so nothing listens and the switch is off.
        assert!(!rig.wake.status().enabled);

        // Armed while the microphone is fine, then the engine goes away (an interruption, a headset
        // unplugged, the Mac waking) and the microphone refuses to open again.
        rig.mic.set_open_error(None);
        rig.wake.set(true).unwrap();
        rig.wake.voice().expect("the rig has a machine").shutdown();
        rig.mic.set_open_error(Some(VoiceError::Failed("The microphone could not start.".into())));
        rig.wake.resume();
        let status = rig.wake.status();
        assert!(!status.available, "a microphone that refused must be visible in the panel");
        assert_eq!(status.reason.as_deref(), Some("The microphone could not start."));

        // Whatever made it refuse goes away: the retry listens again without the user's help.
        let opened = rig.mic.log.lock().unwrap().opened;
        rig.mic.set_open_error(None);
        wait_until("the retry arms the microphone again", || rig.mic.log.lock().unwrap().opened > opened);
        assert!(rig.wake.status().available, "the switch stops warning once it listens again");
    }

    #[test]
    fn turning_the_detector_off_stops_the_pending_retries() {
        let rig = wake_rig("retry-off", true);
        rig.wake.set(true).unwrap();
        // The engine goes away and the device refuses to open a new one: the resume schedules a
        // retry, which the switch going off must cancel.
        rig.wake.voice().expect("the rig has a machine").shutdown();
        rig.mic.set_open_error(Some(VoiceError::Failed("The microphone could not start.".into())));
        rig.wake.resume();
        rig.wake.set(false).unwrap();
        let opened = rig.mic.log.lock().unwrap().opened;
        rig.mic.set_open_error(None);
        std::thread::sleep(std::time::Duration::from_millis(1_400));
        assert_eq!(rig.mic.log.lock().unwrap().opened, opened, "a switch that is off does not retry");
    }

    #[test]
    fn every_command_is_also_a_registered_handler() {
        // The permission and the handler are two different lists: a command granted but not
        // registered answers "Command … not found", which is how wake_phrase_set first shipped.
        let handlers = include_str!("../shell.rs");
        for command in COMMANDS {
            assert!(handlers.contains(&format!("voice::{command}")), "{command} is not in generate_handler");
        }
    }

    #[test]
    fn a_failed_detector_is_logged_too() {
        let rig = wake_rig("failed", true);
        *rig.mic.open_error.lock().unwrap() = Some(VoiceError::NoMicrophone);
        rig.wake.startup(true);
        let log = std::fs::read_to_string(rig.dir.join("wake-word.log")).unwrap();
        let line: serde_json::Value = serde_json::from_str(log.lines().next().unwrap()).unwrap();
        assert_eq!(line["event"], serde_json::json!("failed"));
        assert!(line["reason"].as_str().unwrap().contains("No microphone"), "{line}");
    }
}
