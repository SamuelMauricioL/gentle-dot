//! The wake word "hey dot" (S30.7, S1-S7): saying it starts the note the panel's mic button
//! starts, without touching anything.
//!
//! The detector reads the capture the microphone already opens (S2): the machine arms a listener
//! on that stream and the microphone hands it the same 16 kHz PCM it hands a recording. Nothing
//! here opens a device, spawns a helper or listens on a second engine.
//!
//! `ParakeetWake` is the only detector: the local model, already installed for dictation, listens
//! for the phrase in its own transcript. The phrase saved in the settings is therefore the only
//! thing that can start a note — no second, template-based detector is left to match an older
//! recording. `FakeWake` plays detections on demand so every rule is tested on every platform
//! without a microphone.

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
    /// The variant the recogniser wrote, when the match came from a transcript ("oye dottie").
    /// None for a reference match, which never sees text.
    pub variant: Option<String>,
}

/// Matches wake words in the PCM the microphone delivers. `accept` runs on the audio thread, so
/// it must not block; `reset` runs wherever the machine is disarming.
pub trait WakeWord: Send + Sync {
    /// More audio; `Some` when a wake word matched in what has been heard so far.
    fn accept(&self, samples: &[f32]) -> Option<Detection>;
    /// Drops audio buffered but not yet matched, so the next capture starts clean.
    fn reset(&self);
}

/// (S30.7). Played once per note, at the start only, through the system's own output — it reaches
/// headphones like any other system sound.
pub trait Chime: Send + Sync {
    fn note_started(&self);
}

/// The platforms without a system sound to borrow: nothing is played.
pub struct Silent;

impl Chime for Silent {
    fn note_started(&self) {}
}

/// `Tink`, one of the system sounds macOS already ships: no asset of our own, routed to the
/// current output device.
#[cfg(target_os = "macos")]
pub struct SystemChime;

#[cfg(target_os = "macos")]
impl Chime for SystemChime {
    fn note_started(&self) {
        use objc2_app_kit::NSSound;
        use objc2_foundation::NSString;
        // A shared AppKit instance that outlives the call; playing from a worker thread is what
        // the system sound is for.
        if let Some(sound) = NSSound::soundNamed(&NSString::from_str("Tink")) {
            sound.play();
        }
    }
}

/// The chime this build plays, if the platform has one.
pub fn platform_chime() -> std::sync::Arc<dyn Chime> {
    #[cfg(target_os = "macos")]
    return std::sync::Arc::new(SystemChime);
    #[cfg(not(target_os = "macos"))]
    std::sync::Arc::new(Silent)
}

/// The detector's name for the phrase, and the model name the log records.
pub const WAKE_WORD: &str = "oye-doti";

/// The spike's window: 1.5 s at 16 kHz, long enough for "oye dot" with the air around it.
pub const WINDOW: usize = SAMPLE_RATE as usize * 3 / 2;
/// The overlap between windows: 0.5 s, so a phrase said across a boundary is still heard whole.
pub const HOP: usize = SAMPLE_RATE as usize / 2;

/// The words the detector listens for, normalised once: the panel may type any phrase, so the
/// rule cannot be a table of spellings (S30.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Phrase {
    /// What the panel shows and the log records: the typed phrase, trimmed and single-spaced.
    pub text: String,
    /// The same words, folded for matching.
    words: Vec<String>,
}

impl Phrase {
    /// The phrase as typed, or the default when it is not usable.
    pub fn typed(text: &str) -> Self {
        let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
        let words: Vec<String> = normalised(&text).split_whitespace().map(folded).collect();
        if words.is_empty() {
            return Phrase::default();
        }
        Phrase { text: if text.is_empty() { crate::config::DEFAULT_WAKE_PHRASE.to_string() } else { text }, words }
    }

    /// The variant a transcript says, or none. The words must appear together, in order, anywhere
    /// in the window: "bueno, oye doti" is the command too.
    pub fn said(&self, transcript: &str) -> Option<String> {
        let text = normalised(transcript);
        let words: Vec<String> = text.split_whitespace().map(folded).collect();
        let size = self.words.len();
        if size == 0 || words.len() < size {
            return None;
        }
        words.windows(size).find(|window| *window == self.words.as_slice()).map(|_| self.text.clone())
    }
}

impl Default for Phrase {
    fn default() -> Self {
        Phrase::typed(crate::config::DEFAULT_WAKE_PHRASE)
    }
}

/// One word folded for comparison: repeated letters collapse ("dottie" → "dotie"), a final "ie"
/// becomes "i", and "y" is "i", because a recogniser writes the same name several ways.
fn folded(word: &str) -> String {
    let mut collapsed = String::with_capacity(word.len());
    for c in word.chars() {
        if collapsed.chars().last() != Some(c) {
            collapsed.push(c);
        }
    }
    let mut folded: String = collapsed.chars().map(|c| if c == 'y' { 'i' } else { c }).collect();
    if folded.len() > 2 && folded.ends_with("ie") {
        folded.truncate(folded.len() - 1);
    }
    folded
}

/// Whether a transcript says `phrase`. Kept for the tests and for callers that only need a yes.
pub fn says_wake_word(transcript: &str, phrase: &Phrase) -> Option<String> {
    phrase.said(transcript)
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
/// Why the wake word cannot listen: the local model is the only detector, so without it there is
/// nothing to hear the phrase with.
pub fn parakeet_absent() -> String {
    "The wake word needs the local speech model: install it from Settings, Voz, and turn the wake word on again.".to_string()
}

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
    /// What to listen for, so the panel can change it without a rebuild.
    phrase: Phrase,
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
    pub fn load(dir: &Path, log: PathBuf, phrase: Phrase) -> Result<Self, String> {
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
                phrase,
            }),
        })
    }

    #[cfg(test)]
    fn with_engine(engine: Arc<dyn Engine>) -> Self {
        let shared = Shared {
            engine,
            log: PathBuf::new(),
            buffer: Mutex::new(Vec::new()),
            decoding: AtomicBool::new(false),
            pending: Mutex::new(None),
            phrase: Phrase::default(),
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
            // Silence, other words, or a decode that failed: the next window asks again, so a
            // failed decode costs one window and not the listener.
            if let Ok(text) = heard {
                if let Some(variant) = shared.phrase.said(&text) {
                    *lock(&shared.pending) =
                        Some(Detection { name: WAKE_WORD.to_string(), score: 1.0, variant: Some(variant) });
                }
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
            self.detect_variant(score, None);
        }

        pub fn detect_variant(&self, score: f32, variant: Option<&str>) {
            let variant = variant.map(str::to_string);
            self.detections.lock().unwrap().push(Detection { name: WAKE_WORD.into(), score, variant });
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
        // The default phrase, in the spellings the recogniser returns for "Doti": doubled
        // letters, a final "ie" and "y" for "i" all fold to the same word.
        let phrase = Phrase::typed(crate::config::DEFAULT_WAKE_PHRASE);
        for said in [
            "oye doti",
            "Oye Doti",
            "oye doty",
            "oye dottie",
            "oye dotti",
            "¿oye, doti?",
            "  oye   dottie. ",
            "bueno, oye doti",
            "oye doti oye",
        ] {
            assert_eq!(phrase.said(said).as_deref(), Some("oye Doti"), "{said}");
        }
    }

    #[test]
    fn another_phrase_is_heard_just_as_well() {
        // Any phrase the panel types: the rule is the words, not a table of spellings.
        let phrase = Phrase::typed("hey dot");
        assert_eq!(phrase.said("Hey, dott.").as_deref(), Some("hey dot"));
        assert_eq!(phrase.said("hola hey dot"), Some("hey dot".into()));
        assert_eq!(phrase.said("hey dottie"), None, "one more letter is another word");

        let three = Phrase::typed("atencion asistente ya");
        assert_eq!(three.said("la atencion asistente ya por favor"), Some("atencion asistente ya".into()));
        assert_eq!(Phrase::typed("").text, crate::config::DEFAULT_WAKE_PHRASE, "empty falls back");
    }

    #[test]
    fn anything_else_is_not_the_phrase() {
        // Only the opener the user chose counts: "hola doti" and "hey doti" are not the phrase, and
        // neither is one word alone or a different name, so a conversation does not start a note.
        let phrase = Phrase::typed(crate::config::DEFAULT_WAKE_PHRASE);
        for near in ["", "oye", "doti", "oye punto", "hola doti", "hey doti", "ey doti", "doti oye", "oye dote"] {
            assert_eq!(phrase.said(near), None, "{near}");
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
        let engine = ScriptedEngine::gated("oye doti", gate.clone());
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
        let wake = ParakeetWake::with_engine(ScriptedEngine::saying("Oye, dottie."));
        assert_eq!(wake.accept(&[0.1; WINDOW]), None, "the match is not in yet");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
        let mut heard = wake.accept(&[0.0; 160]);
        while heard.is_none() && std::time::Instant::now() < deadline {
            heard = wake.accept(&[0.0; 160]);
        }
        assert_eq!(heard, Some(Detection { name: WAKE_WORD.into(), score: 1.0, variant: Some("oye Doti".into()) }));
    }

    #[test]
    fn reset_drops_a_match_waiting_for_the_next_callback() {
        let wake = ParakeetWake::with_engine(ScriptedEngine::saying("nada"));
        wake.accept(&[0.0; 160]);
        *lock(&wake.shared.pending) = Some(Detection { name: WAKE_WORD.into(), score: 1.0, variant: None });
        wake.reset();
        assert_eq!(wake.buffered(), 0, "the buffered audio goes with it");
        assert_eq!(wake.accept(&[0.0; 160]), None, "the match goes with it");
    }

    #[test]
    fn a_model_that_is_not_there_is_an_error_not_a_panic() {
        let dir = std::env::temp_dir().join(format!("gentle-dot-no-parakeet-{}", std::process::id()));
        let error = ParakeetWake::load(&dir, PathBuf::from("/tmp/wake-word-test.log"), Phrase::default()).err().expect("a missing model is an error");
        assert!(error.contains(&dir.display().to_string()), "{error}");
    }

    #[test]
    fn a_scripted_detection_is_played_once_and_the_audio_is_kept() {
        let wake = FakeWake::default();
        wake.detect(0.51);
        assert_eq!(wake.accept(&[0.1, 0.2]), Some(Detection { name: WAKE_WORD.into(), score: 0.51, variant: None }));
        assert_eq!(wake.accept(&[0.3]), None);
        assert_eq!(wake.heard(), 3);
        wake.reset();
        assert_eq!(wake.resets(), 1);
    }

    #[test]
    fn only_the_saved_phrase_wakes_the_app() {
        // The phrase in the settings is the whole rule: no template of an older recording is left
        // to match something the user did not type, and no near phrase takes its place.
        let phrase = Phrase::typed("oye Doti");
        assert!(phrase.said("oye doti").is_some());
        for other in ["oye dot", "hey doti", "oye", "doti", "oye dote"] {
            assert_eq!(phrase.said(other), None, "{other}");
        }
        // Saving another phrase moves the command instead of adding one.
        let changed = Phrase::typed("hey dot");
        assert_eq!(changed.said("oye doti"), None, "the old phrase stops working");
        assert_eq!(changed.said("hey dot").as_deref(), Some("hey dot"));
    }
}
