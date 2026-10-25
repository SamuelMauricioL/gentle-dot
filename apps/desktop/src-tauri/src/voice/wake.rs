//! The wake word "oye dot" (S30.7, S1-S7): saying it starts the note the panel's mic button
//! starts, without touching anything.
//!
//! The detector reads the capture the microphone already opens (S2): the machine arms a listener
//! on that stream and the microphone hands it the same 16 kHz PCM it hands a recording. Nothing
//! here opens a device, spawns a helper or listens on a second engine.
//!
//! `RustpotterWake` is the real detector (rustpotter, macOS only, the platform with that capture);
//! `ParakeetWake` is the spike: the local model, already installed for dictation, listens for the
//! phrase in its own transcript instead of a trained reference; `FakeWake` plays detections on
//! demand so every rule is tested on every platform without a microphone.

use super::audio::SAMPLE_RATE;
use super::parakeet::{self, Engine};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

/// One wake word matched in the stream.
#[derive(Debug, Clone, PartialEq)]
pub struct Detection {
    /// The key the reference was loaded under (`WAKE_WORD`), as the detector reports it.
    pub name: String,
    pub score: f32,
}

/// Matches wake words in the PCM the microphone delivers. `accept` runs on the audio thread, so
/// it must not block; `reset` runs wherever the machine is disarming.
pub trait WakeWord: Send + Sync {
    /// More audio; `Some` when a wake word matched in what has been heard so far.
    fn accept(&self, samples: &[f32]) -> Option<Detection>;
    /// Drops audio buffered but not yet matched, so the next capture starts clean.
    fn reset(&self);
}

/// The reference's file name. It is a voice signature, so it lives in the data dir (S4), is never
/// bundled with the build, is never committed, and `*.rpw` is in `.gitignore`.
pub const MODEL_FILE: &str = "oye-dot.rpw";
/// The detector's name for the phrase, and the model name the log records.
pub const WAKE_WORD: &str = "oye-dot";
/// The operating point measured on this machine (S7, L4): four samples, leave-one-out margin
/// 0.062, and no usable threshold from 0.50 up.
/// TEMPORARY operating point for a diagnostic run: low enough that any match shows up in
/// `wake-word.log` with its score. The measured point was 0.45 / 5; restore or recalibrate once
/// the score is known.
pub const THRESHOLD: f32 = 0.05;
pub const MIN_SCORES: usize = 1;

/// `<data dir>/wake-word/oye-dot.rpw`, next to `config.json` (S4).
pub fn model_path(data_dir: &Path) -> PathBuf {
    data_dir.join("wake-word").join(MODEL_FILE)
}

/// Why the detector has no reference to listen for, naming the file the user has to put there:
/// the one thing the app cannot do for him (the file is his own voice, S4).
pub fn missing(path: &Path) -> String {
    format!(
        "The wake word's voice reference is missing at {}. Put {MODEL_FILE} there, then turn the wake word on again.",
        path.display()
    )
}

/// The spike's window: 1.5 s at 16 kHz, long enough for "oye dot" with the air around it.
pub const WINDOW: usize = SAMPLE_RATE as usize * 3 / 2;
/// The overlap between windows: 0.5 s, so a phrase said across a boundary is still heard whole.
pub const HOP: usize = SAMPLE_RATE as usize / 2;

/// The ways the speaker opens the phrase, and the ways Parakeet writes the name down. Matching the
/// pair instead of a fixed list keeps the command usable when the model hears "hola dock" or
/// "hey dot": the two words are the command, the exact spelling is not. Spanish does not end a
/// word on /t/, which is why "dote" is here.
const OPENERS: [&str; 7] = ["oye", "oy", "hoy", "ey", "hey", "hola", "ola"];
const NAMES: [&str; 7] = ["dot", "dock", "dots", "dote", "doc", "dott", "dok"];

/// Whether a transcript says the phrase. Pure, so the rule is tested without a model or a mic.
/// The pair may sit anywhere in the window: "hola, oye dot" is the command too.
pub fn says_wake_word(transcript: &str) -> bool {
    let text = normalised(transcript);
    let words: Vec<&str> = text.split_whitespace().collect();
    words
        .windows(2)
        .any(|pair| OPENERS.contains(&pair[0]) && NAMES.contains(&pair[1]))
}

/// Lowercase, no accents, punctuation to spaces, single spaces: "Oye,  dót." is "oye dot".
fn normalised(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.to_lowercase().chars() {
        match c {
            'á' | 'à' | 'â' | 'ä' => out.push('a'),
            'é' | 'è' | 'ê' | 'ë' => out.push('e'),
            'í' | 'ì' | 'î' | 'ï' => out.push('i'),
            'ó' | 'ò' | 'ô' | 'ö' => out.push('o'),
            'ú' | 'ù' | 'û' | 'ü' => out.push('u'),
            c if c.is_alphanumeric() => out.push(c),
            _ => out.push(' '),
        }
    }
    out.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Why the spike cannot listen: the model directory the app downloads into is not there.
fn parakeet_missing(dir: &Path) -> String {
    format!(
        "The local voice model's files are missing at {}, so it cannot listen for the phrase. Download the local model again.",
        dir.display()
    )
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The spike: Parakeet listens for the phrase instead of a reference. It buffers the PCM of the
/// microphone's capture into windows of `WINDOW`, decodes each one on a thread of its own, and
/// looks for the phrase in the transcript, keeping `HOP` of overlap.
///
/// The trade-off it accepts: a whole window is decoded, so a match surfaces on a callback after
/// the audio that heard it, and only one window is decoded at a time. Correctness is the question
/// here, latency is not.
pub struct ParakeetWake {
    shared: Arc<Shared>,
}

struct Shared {
    engine: Arc<dyn Engine>,
    /// Where each window's transcript is written; empty in tests, which need no log.
    log: PathBuf,
    /// Audio heard but not yet a full window.
    buffer: Mutex<Vec<f32>>,
    /// A decode in flight: the next window waits for it instead of queueing behind it.
    decoding: AtomicBool,
    /// The match a decode left behind, handed over on the next callback.
    pending: Mutex<Option<Detection>>,
}

/// Writes one window's transcript to the wake log. Without it a spike that never matches says
/// nothing about what the recogniser actually heard.
fn note(log: &Path, text: &str) {
    if log.as_os_str().is_empty() {
        return;
    }
    let at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default();
    let line = serde_json::json!({ "at": at, "event": "transcript", "text": text }).to_string() + "\n";
    if let Ok(mut file) = std::fs::OpenOptions::new().create(true).append(true).open(log) {
        use std::io::Write;
        let _ = file.write_all(line.as_bytes());
    }
}

/// Clears the in-flight flag however the decode ends, a panic included, so one bad window cannot
/// leave the listener deaf for good.
struct Decoding(Arc<Shared>);

impl Drop for Decoding {
    fn drop(&mut self) {
        self.0.decoding.store(false, Ordering::SeqCst);
    }
}

impl ParakeetWake {
    /// Loads the model in `dir` (seconds, so never on the audio thread). A directory that is not
    /// there is an error the caller shows, never a panic.
    pub fn load(dir: &Path, log: PathBuf) -> Result<Self, String> {
        if !dir.is_dir() {
            return Err(parakeet_missing(dir));
        }
        let engine = parakeet::load(dir)?;
        Ok(ParakeetWake {
            shared: Arc::new(Shared {
                engine,
                log,
                buffer: Mutex::new(Vec::new()),
                decoding: AtomicBool::new(false),
                pending: Mutex::new(None),
            }),
        })
    }

    fn with_engine(engine: Arc<dyn Engine>) -> Self {
        let shared = Shared {
            engine,
            log: PathBuf::new(),
            buffer: Mutex::new(Vec::new()),
            decoding: AtomicBool::new(false),
            pending: Mutex::new(None),
        };
        ParakeetWake { shared: Arc::new(shared) }
    }

    /// Decodes one window away from the audio thread and leaves a match in the pending slot.
    fn request(&self, window: Vec<f32>) {
        let shared = self.shared.clone();
        let spawned = std::thread::Builder::new().name("wake-parakeet".into()).spawn(move || {
            let _busy = Decoding(shared.clone());
            let heard = shared.engine.transcribe(&window);
            note(&shared.log, heard.as_deref().unwrap_or(""));
            match heard {
                Ok(text) if says_wake_word(&text) => {
                    *lock(&shared.pending) = Some(Detection { name: WAKE_WORD.to_string(), score: 1.0 });
                }
                // Silence, other words, or a decode that failed: the next window asks again, so a
                // failed decode costs one window and not the listener. There is no log to reach
                // from here (`Wake`'s lives in app.rs), so it stays quiet and keeps listening.
                Ok(_) | Err(_) => {}
            }
        });
        if spawned.is_err() {
            // No thread, no decode: the flag stays clear so the next window tries again.
            self.shared.decoding.store(false, Ordering::SeqCst);
        }
    }

    #[cfg(test)]
    fn buffered(&self) -> usize {
        lock(&self.shared.buffer).len()
    }
}

impl WakeWord for ParakeetWake {
    fn accept(&self, samples: &[f32]) -> Option<Detection> {
        // Taken before this audio is looked at, so a match always comes from an earlier window:
        // the decode never runs here, and this thread never waits for one.
        let found = lock(&self.shared.pending).take();
        let mut buffer = lock(&self.shared.buffer);
        buffer.extend_from_slice(samples);
        // While a decode runs only the newest window is kept, so a slow decode cannot grow the
        // buffer without bound.
        if buffer.len() > WINDOW * 2 {
            let excess = buffer.len() - WINDOW;
            buffer.drain(..excess);
        }
        if buffer.len() >= WINDOW && !self.shared.decoding.swap(true, Ordering::SeqCst) {
            let window = buffer[..WINDOW].to_vec();
            // The hop stays in front of the next window, so a phrase said across the boundary is
            // heard whole by the next one instead of half by each.
            buffer.drain(..WINDOW - HOP);
            drop(buffer);
            self.request(window);
        }
        found
    }

    fn reset(&self) {
        lock(&self.shared.buffer).clear();
        *lock(&self.shared.pending) = None;
    }
}

#[cfg(target_os = "macos")]
pub use real::RustpotterWake;

#[cfg(target_os = "macos")]
mod real {
    use super::{missing, Detection, WakeWord, MIN_SCORES, THRESHOLD, WAKE_WORD};
    use rustpotter::{Rustpotter, RustpotterConfig};
    use std::path::Path;
    use std::sync::Mutex;

    /// rustpotter wants whole frames (`get_samples_per_frame`, 480 samples = 30 ms at 16 kHz)
    /// and the microphone's tap delivers about 21 ms, so chunks are buffered until a frame is
    /// full. The detector is mutexed because the trait is shared: `accept` only ever runs on the
    /// audio thread, one chunk at a time.
    pub struct RustpotterWake {
        pending: Mutex<Pending>,
    }

    struct Pending {
        detector: Rustpotter,
        samples: Vec<f32>,
    }

    impl RustpotterWake {
        /// Loads the reference at `path`, configured with the measured operating point (S7). A
        /// missing or unreadable file is reported with the path: nothing else tells the user
        /// where the reference has to be (S4).
        pub fn load(path: &Path) -> Result<Self, String> {
            if !path.is_file() {
                return Err(missing(path));
            }
            let mut config = RustpotterConfig::default();
            config.detector.threshold = THRESHOLD;
            config.detector.min_scores = MIN_SCORES;
            let mut detector = Rustpotter::new(&config)?;
            let file = path
                .to_str()
                .ok_or_else(|| format!("The wake word reference at {} is not a valid path.", path.display()))?;
            detector
                .add_wakeword_from_file(WAKE_WORD, file)
                .map_err(|error| format!("The wake word reference at {} could not be read: {error}", path.display()))?;
            Ok(RustpotterWake { pending: Mutex::new(Pending { detector, samples: Vec::new() }) })
        }
    }

    impl WakeWord for RustpotterWake {
        fn accept(&self, samples: &[f32]) -> Option<Detection> {
            let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            let frame = pending.detector.get_samples_per_frame();
            pending.samples.extend_from_slice(samples);
            // Every complete frame is processed, not one per callback: the capture delivers chunks
            // larger than a frame, so one frame per chunk falls behind for good and the detector
            // never reaches the phrase just spoken.
            while pending.samples.len() >= frame {
                let audio: Vec<f32> = pending.samples.drain(..frame).collect();
                let Some(detection) = pending.detector.process_samples(audio) else { continue };
                // The detector keeps its own history: a phrase already matched must not match
                // again on the audio that follows it.
                pending.samples.clear();
                pending.detector.reset();
                return Some(Detection { name: detection.name, score: detection.score });
            }
            None
        }

        fn reset(&self) {
            let mut pending = self.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
            pending.samples.clear();
            pending.detector.reset();
        }
    }
}

#[cfg(test)]
pub use fake::FakeWake;

#[cfg(test)]
mod fake {
    use super::{Detection, WakeWord, WAKE_WORD};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    /// A detector a test scripts: it plays the detections it was given, one per `accept`, and
    /// keeps the audio that reached it so a test can check the stream it was fed.
    #[derive(Default)]
    pub struct FakeWake {
        /// Detections still to play, in order.
        pub detections: Mutex<Vec<Detection>>,
        /// Every sample that reached the detector.
        pub heard: Mutex<Vec<f32>>,
        /// How many times the machine dropped what was buffered.
        pub resets: AtomicUsize,
    }

    impl FakeWake {
        /// Plays this score on the next audio the detector hears.
        pub fn detect(&self, score: f32) {
            self.detections.lock().unwrap().push(Detection { name: WAKE_WORD.into(), score });
        }

        pub fn heard(&self) -> usize {
            self.heard.lock().unwrap().len()
        }

        pub fn resets(&self) -> usize {
            self.resets.load(Ordering::SeqCst)
        }
    }

    impl WakeWord for FakeWake {
        fn accept(&self, samples: &[f32]) -> Option<Detection> {
            self.heard.lock().unwrap().extend_from_slice(samples);
            let mut detections = self.detections.lock().unwrap();
            if detections.is_empty() {
                None
            } else {
                Some(detections.remove(0))
            }
        }

        fn reset(&self) {
            self.resets.fetch_add(1, Ordering::SeqCst);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::fake::Gate;
    use std::sync::atomic::AtomicUsize;

    /// A model that answers what the test told it to, and counts the windows it decoded.
    struct ScriptedEngine {
        text: Mutex<String>,
        decodes: AtomicUsize,
        /// When set, a decode waits here, as a slow model does.
        gate: Option<Arc<Gate>>,
    }

    impl ScriptedEngine {
        fn saying(text: &str) -> Arc<Self> {
            Arc::new(ScriptedEngine { text: Mutex::new(text.into()), decodes: AtomicUsize::new(0), gate: None })
        }

        fn gated(text: &str, gate: Arc<Gate>) -> Arc<Self> {
            Arc::new(ScriptedEngine { text: Mutex::new(text.into()), decodes: AtomicUsize::new(0), gate: Some(gate) })
        }

        fn decodes(&self) -> usize {
            self.decodes.load(Ordering::SeqCst)
        }
    }

    impl Engine for ScriptedEngine {
        fn transcribe(&self, _samples: &[f32]) -> Result<String, String> {
            self.decodes.fetch_add(1, Ordering::SeqCst);
            if let Some(gate) = &self.gate {
                gate.wait();
            }
            Ok(lock(&self.text).clone())
        }
    }

    /// Waits for a background reaction (the decode runs on a spawned thread) before checking it.
    fn wait_until(what: &str, mut done: impl FnMut() -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        while !done() && std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(1));
        }
        assert!(done(), "{what}");
    }

    #[test]
    fn the_phrase_as_it_is_really_said_matches() {
        for phrase in [
            "oye dot",
            "Oye, dot",
            "  oye   dot. ",
            "¿oye dot?",
            "hoy dot",
            "oye dock",
            "oye dots",
            "oye dote",
            "hola dot",
            "hey dot",
            "ey dock",
            "oy dot",
            "ola dote",
            "oye doc",
            // The pair anywhere in the window, which is how a person actually speaks.
            "hola, oye dot",
            "oye dot oye",
        ] {
            assert!(says_wake_word(phrase), "{phrase}");
        }
    }

    #[test]
    fn anything_else_is_not_the_phrase() {
        // One word alone, the pair inverted, or a different name: no match, so a conversation that
        // happens to contain "oye" or "dot" does not start a note.
        for near in ["", "oye", "dot", "oye punto", "hola", "dot oye", "oye dota", "donde esta el dot"] {
            assert!(!says_wake_word(near), "{near}");
        }
    }

    #[test]
    fn less_than_a_window_asks_for_no_decode_and_the_overlap_is_kept() {
        let engine = ScriptedEngine::saying("silencio");
        let wake = ParakeetWake::with_engine(engine.clone());
        wake.accept(&[0.0; WINDOW - HOP]);
        wake.accept(&[0.0; HOP - 1]);
        assert_eq!(wake.buffered(), WINDOW - 1);
        assert_eq!(engine.decodes(), 0, "no full window, no decode");
        wake.accept(&[0.0; HOP]);
        wait_until("the full window reaches the decode", || engine.decodes() == 1);
        // Everything past the window's first `WINDOW - HOP` samples stays: the last 0.5 s of the
        // decoded window plus the audio that arrived after it.
        assert_eq!(wake.buffered(), HOP * 2 - 1);
    }

    #[test]
    fn a_slow_decode_never_blocks_the_audio_thread_or_asks_again() {
        let gate = Arc::new(Gate::default());
        let engine = ScriptedEngine::gated("oye dot", gate.clone());
        let wake = ParakeetWake::with_engine(engine.clone());
        assert_eq!(wake.accept(&[0.1; WINDOW]), None, "nothing decoded yet");
        wait_until("the window reaches the decode", || engine.decodes() == 1);
        for _ in 0..10 {
            assert_eq!(wake.accept(&[0.1; 1_600]), None, "the audio thread does not wait for the decode");
        }
        assert_eq!(engine.decodes(), 1, "one decode at a time, no queue behind it");
        gate.release();
    }

    #[test]
    fn a_match_decoded_off_the_audio_thread_surfaces_on_a_later_callback() {
        let wake = ParakeetWake::with_engine(ScriptedEngine::saying("Oye, dot."));
        assert_eq!(wake.accept(&[0.1; WINDOW]), None, "the match is not in yet");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut heard = wake.accept(&[0.0; 160]);
        while heard.is_none() && std::time::Instant::now() < deadline {
            heard = wake.accept(&[0.0; 160]);
        }
        assert_eq!(heard, Some(Detection { name: WAKE_WORD.into(), score: 1.0 }));
    }

    #[test]
    fn reset_drops_a_match_waiting_for_the_next_callback() {
        let wake = ParakeetWake::with_engine(ScriptedEngine::saying("nada"));
        wake.accept(&[0.0; 160]);
        *lock(&wake.shared.pending) = Some(Detection { name: WAKE_WORD.into(), score: 1.0 });
        wake.reset();
        assert_eq!(wake.buffered(), 0, "the buffered audio goes with it");
        assert_eq!(wake.accept(&[0.0; 160]), None, "the match goes with it");
    }

    #[test]
    fn a_model_that_is_not_there_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("gentle-dot-no-parakeet-{}", std::process::id()));
        let error = ParakeetWake::load(&dir, PathBuf::from("/tmp/wake-word-test.log")).err().expect("a missing model is an error");
        assert!(error.contains(&dir.display().to_string()), "{error}");
    }

    #[test]
    fn the_reference_lives_under_the_data_dir() {
        assert_eq!(model_path(Path::new("/data/dot")), Path::new("/data/dot").join("wake-word").join("oye-dot.rpw"));
    }

    #[test]
    fn a_missing_reference_is_reported_with_the_path_and_the_file_name() {
        let reason = missing(Path::new("/data/dot/wake-word/oye-dot.rpw"));
        assert!(reason.contains("/data/dot/wake-word/oye-dot.rpw"), "{reason}");
        assert!(reason.contains(MODEL_FILE), "{reason}");
    }

    #[test]
    fn the_operating_point_is_the_measured_one() {
        // Diagnostic run: the temporary point, so a match is recorded with its score. Restore or
        // recalibrate once the score is known (see THRESHOLD's own note).
        assert_eq!((THRESHOLD, MIN_SCORES), (0.05, 1));
    }

    #[test]
    fn a_scripted_detection_is_played_once_and_the_audio_is_kept() {
        let wake = FakeWake::default();
        wake.detect(0.51);
        assert_eq!(wake.accept(&[0.1, 0.2]), Some(Detection { name: WAKE_WORD.into(), score: 0.51 }));
        assert_eq!(wake.accept(&[0.3]), None);
        assert_eq!(wake.heard(), 3);
        wake.reset();
        assert_eq!(wake.resets(), 1);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn loading_a_reference_that_is_not_there_names_the_file() {
        let path = std::env::temp_dir().join(format!("gentle-dot-no-wake-{}", std::process::id())).join(MODEL_FILE);
        let error = RustpotterWake::load(&path).err().expect("a missing reference is an error");
        assert!(error.contains("missing"), "{error}");
        assert!(error.contains(path.to_str().unwrap()), "{error}");
    }
}
