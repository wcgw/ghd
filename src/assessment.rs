//! Assessments: voight's estimate of how much of the text a Preview shows was written by an LLM (see
//! `CONTEXT.md`). Never evidence of authorship.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use chrono::{DateTime, Utc};
use tokio::sync::mpsc::UnboundedReceiver;

use crate::inbox::{Listing, ThreadId};

/// The fewest words of prose worth assessing: on fewer, voight's estimate means little.
#[cfg(feature = "voight")]
pub const MIN_WORDS: usize = 50;

/// Where voight runs its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    /// A Mac's GPU.
    Metal,
    /// An NVIDIA GPU.
    Cuda,
}

impl Device {
    /// Whether this ghd was built to run voight there: CUDA with its `cuda` feature, Metal with its `metal` one or
    /// on Apple silicon, where voight always builds with it. Whether the machine has one is another matter.
    pub fn is_built_in(self) -> bool {
        match self {
            Self::Cpu => cfg!(feature = "voight"),
            Self::Metal => cfg!(all(
                feature = "voight",
                any(
                    feature = "metal",
                    all(target_os = "macos", target_arch = "aarch64")
                )
            )),
            Self::Cuda => cfg!(feature = "cuda"),
        }
    }
}

/// As the configuration's `voight` names it.
impl fmt::Display for Device {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Cpu => "cpu",
            Self::Metal => "metal",
            Self::Cuda => "cuda",
        })
    }
}

/// What an Assessment is of: the text a thread's Preview shows, or one of its Changed files, with the activity its
/// Notification had.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub thread: ThreadId,
    pub as_of: DateTime<Utc>,
    /// The Changed file's contents path; `None` for the Preview.
    pub file: Option<String>,
}

/// voight at work on a thread of its own: it loads the model, then assesses the texts it's given, one at a time,
/// in order.
#[derive(Debug)]
pub struct Assessor {
    texts: std::sync::mpsc::Sender<(Key, String)>,
    events: UnboundedReceiver<Event>,
}

/// What voight has to say.
#[derive(Debug)]
// Built without voight, nothing says anything but the UI's tests; the UI still knows what it would hear.
#[cfg_attr(not(feature = "voight"), allow(dead_code))]
pub enum Event {
    /// The model is loaded. `fallback` says why it runs on the CPU rather than the device asked for, if it does.
    Loaded { fallback: Option<String> },
    /// The model couldn't be loaded: there'll be no Assessments this run.
    Failed(String),
    /// The Assessment asked for under `key`; `None` for a text with too little prose to tell.
    Assessed {
        key: Key,
        assessment: Result<Option<Assessment>, String>,
    },
}

/// An Assessment of a text: the share of it an LLM wrote, overall and in each window voight scored.
#[derive(Debug, Clone, PartialEq)]
pub struct Assessment {
    /// Between 0 and 1: the windows' shares, weighted by their words.
    pub share: f64,
    /// In the order they come in the text.
    pub windows: Vec<Window>,
}

/// A window of prose voight scored on its own: paragraphs of about 100 to 400 words together, or part of a longer
/// one, cut where a sentence ends.
#[derive(Debug, Clone, PartialEq)]
pub struct Window {
    /// Where it is in the text voight was given, in bytes.
    pub range: Range<usize>,
    /// The share of it an LLM wrote, between 0 and 1.
    pub share: f64,
}

impl Assessor {
    /// Fetches voight's model and starts loading it on a thread of its own, to run on `device`. Fetching downloads
    /// the model the first time (about 1.7 GB), showing voight's progress on stderr: call it before the terminal UI
    /// takes over the screen. It returns at once otherwise; [`Event::Loaded`] or [`Event::Failed`] says how loading
    /// went.
    ///
    /// # Errors
    ///
    /// Fails if the model can't be fetched.
    #[cfg(feature = "voight")]
    pub async fn start(device: Device) -> anyhow::Result<Self> {
        use anyhow::Context as _;

        let model = voight::Model::Gradient;
        let files = tokio::task::spawn_blocking(move || voight::ModelFiles::fetch(model))
            .await
            .context("fetching voight's model stopped")??;
        let (texts, queued) = std::sync::mpsc::channel();
        let (events_out, events) = tokio::sync::mpsc::unbounded_channel();
        let worker_events = events_out.clone();
        let spawned = std::thread::Builder::new()
            .name("voight".to_owned())
            .spawn(move || work(model, &files, device, &queued, &worker_events));
        if let Err(error) = spawned {
            // Only the receiver can be gone, and it's right here.
            let _ = events_out.send(Event::Failed(format!(
                "couldn't start a thread for voight: {error}"
            )));
        }
        Ok(Self { texts, events })
    }

    /// Asks for the Assessment of `text`, to come as an [`Event::Assessed`] under `key`, after those asked for
    /// before it.
    pub fn assess(&self, key: Key, text: String) {
        // Once voight's thread has stopped, its failure has been said: the text goes unassessed.
        let _ = self.texts.send((key, text));
    }

    /// What voight says next; never `None` while it runs.
    ///
    /// # Cancel Safety
    ///
    /// Cancel safe: an event not taken stays for the next call.
    pub async fn next(&mut self) -> Option<Event> {
        self.events.recv().await
    }
}

/// voight's thread: loads `model` from `files` onto `device`, then assesses the texts `queued`, until the UI is
/// gone. Says how it goes on `events`.
#[cfg(feature = "voight")]
fn work(
    model: voight::Model,
    files: &voight::ModelFiles,
    device: Device,
    queued: &std::sync::mpsc::Receiver<(Key, String)>,
    events: &tokio::sync::mpsc::UnboundedSender<Event>,
) {
    let (device, fallback) = match device {
        Device::Cpu => (voight::Device::Cpu, None),
        Device::Metal => opened(voight::Device::new_metal(0), "Metal"),
        Device::Cuda => opened(voight::Device::new_cuda(0), "CUDA"),
    };
    let detector = match voight::Detector::load(model, files, device) {
        Ok(detector) => detector,
        Err(error) => {
            // The UI is gone if it can't be told: nothing more to do either way.
            let _ = events.send(Event::Failed(format!("{error:#}")));
            return;
        }
    };
    if events.send(Event::Loaded { fallback }).is_err() {
        return;
    }
    for (key, text) in queued {
        let assessment = assess(&detector, &text).map_err(|error| format!("{error:#}"));
        if events.send(Event::Assessed { key, assessment }).is_err() {
            return;
        }
    }
}

/// The device `opening` opened, or the CPU, with why, if it failed: voight's own fallback.
#[cfg(feature = "voight")]
fn opened(
    opening: Result<voight::Device, impl std::fmt::Display>,
    name: &str,
) -> (voight::Device, Option<String>) {
    match opening {
        Ok(device) => (device, None),
        Err(error) => (
            voight::Device::Cpu,
            Some(format!(
                "couldn't use {name} ({error}), so voight runs on the CPU"
            )),
        ),
    }
}

/// The Assessment of `text` by `detector`; `None` if it has fewer than [`MIN_WORDS`] words of prose.
#[cfg(feature = "voight")]
fn assess(detector: &voight::Detector, text: &str) -> anyhow::Result<Option<Assessment>> {
    if !worth_assessing(text) {
        return Ok(None);
    }
    let report = detector.report(text)?;
    Ok(Some(Assessment {
        share: report.overall().value(),
        windows: report
            .windows()
            .map(|(window, llm)| Window {
                range: window.range.clone(),
                share: llm.value(),
            })
            .collect(),
    }))
}

/// Whether `text` has at least [`MIN_WORDS`] words of prose, as voight counts them: code doesn't count.
#[cfg(feature = "voight")]
fn worth_assessing(text: &str) -> bool {
    voight::segments(text)
        .iter()
        .map(|segment| segment.words)
        .sum::<usize>()
        >= MIN_WORDS
}

/// Where an Assessment stands.
#[derive(Debug, Clone, PartialEq)]
pub enum Assessed {
    Assessing,
    /// `None` for a text with too little prose.
    Done(Option<Assessment>),
    /// Why it couldn't be made.
    Failed(String),
}

/// Whether voight's model is ready.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Readiness {
    Loading,
    Ready,
    /// It couldn't be loaded: there'll be no Assessments this run.
    Failed,
}

/// The Assessments made this run, and whether voight is ready to make more.
#[derive(Debug)]
pub struct Assessments {
    readiness: Readiness,
    /// Only the latest activity's, for each thread's Preview and Changed file.
    by_key: HashMap<Key, Assessed>,
}

impl Default for Assessments {
    /// None yet, voight's model loading.
    fn default() -> Self {
        Self {
            readiness: Readiness::Loading,
            by_key: HashMap::new(),
        }
    }
}

impl Assessments {
    pub fn readiness(&self) -> Readiness {
        self.readiness
    }

    /// Notes that voight's model is ready (`true`), or failed to load.
    pub fn loaded(&mut self, ready: bool) {
        self.readiness = if ready {
            Readiness::Ready
        } else {
            Readiness::Failed
        };
    }

    /// Where the Assessment under `key` stands; `None` if it's never been asked for, or only for older activity.
    pub fn get(&self, key: &Key) -> Option<&Assessed> {
        self.by_key.get(key)
    }

    /// Whether voight is ready for another text: its model loaded, and no Assessment under way.
    pub fn can_assess(&self) -> bool {
        self.readiness == Readiness::Ready
            && !self
                .by_key
                .values()
                .any(|assessed| *assessed == Assessed::Assessing)
    }

    /// Notes that the Assessment under `key` is under way, in place of any of the same text for older activity.
    pub fn assessing(&mut self, key: Key) {
        self.by_key.retain(|kept, assessed| {
            kept.thread != key.thread || kept.file != key.file || *assessed == Assessed::Assessing
        });
        self.by_key.insert(key, Assessed::Assessing);
    }

    /// Takes in the Assessment made under `key`.
    pub fn assessed(&mut self, key: Key, assessment: Result<Option<Assessment>, String>) {
        let assessed = match assessment {
            Ok(assessment) => Assessed::Done(assessment),
            Err(why) => Assessed::Failed(why),
        };
        self.by_key.insert(key, assessed);
    }

    /// Forgets `thread`'s Assessments, so that they're made again. One under way still arrives, and is taken in.
    pub fn forget(&mut self, thread: ThreadId) {
        self.by_key
            .retain(|key, assessed| key.thread != thread || *assessed == Assessed::Assessing);
    }

    /// Forgets the Assessments of threads neither of `listing`'s lists holds, but one under way.
    pub fn retain_in(&mut self, listing: &Listing) {
        self.by_key.retain(|key, assessed| {
            listing.contains(key.thread) || *assessed == Assessed::Assessing
        });
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;

    use super::*;

    fn key(thread: u64) -> Key {
        Key {
            thread: ThreadId(thread),
            as_of: "2026-10-22T12:00:00Z".parse().expect("valid RFC 3339"),
            file: None,
        }
    }

    #[test]
    fn one_assessment_at_a_time_once_loaded() {
        let mut assessments = Assessments::default();
        assert!(!assessments.can_assess(), "still loading");

        assessments.loaded(true);
        assert!(assessments.can_assess());
        assessments.assessing(key(1));
        assert!(!assessments.can_assess(), "one under way");
        assert_eq!(assessments.get(&key(1)), Some(&Assessed::Assessing));

        let made = Assessment {
            share: 0.73,
            windows: vec![Window {
                range: 0..120,
                share: 0.73,
            }],
        };
        assessments.assessed(key(1), Ok(Some(made.clone())));
        assert!(assessments.can_assess());
        assert_eq!(assessments.get(&key(1)), Some(&Assessed::Done(Some(made))));
    }

    #[test]
    fn failed_model_assesses_nothing() {
        let mut assessments = Assessments::default();

        assessments.loaded(false);

        assert_eq!(assessments.readiness(), Readiness::Failed);
        assert!(!assessments.can_assess());
    }

    #[test]
    fn newer_activity_wants_a_new_assessment() {
        let mut assessments = Assessments::default();
        assessments.assessed(key(1), Ok(None));

        let newer = Key {
            as_of: key(1).as_of + TimeDelta::minutes(5),
            ..key(1)
        };

        assert_eq!(assessments.get(&newer), None);
    }

    #[test]
    fn assessment_under_way_is_not_forgotten() {
        let mut assessments = Assessments::default();
        assessments.loaded(true);
        assessments.assessing(key(1));
        assessments.assessed(key(2), Err("too long".to_owned()));

        assessments.forget(ThreadId(1));
        assessments.forget(ThreadId(2));
        assessments.retain_in(&Listing::default());

        assert_eq!(assessments.get(&key(1)), Some(&Assessed::Assessing));
        assert_eq!(assessments.get(&key(2)), None);
    }

    #[cfg(feature = "voight")]
    #[test]
    fn only_enough_prose_is_worth_assessing() {
        let words = |count: usize| vec!["word"; count].join(" ");
        assert!(worth_assessing(&words(MIN_WORDS)));
        assert!(!worth_assessing(&words(MIN_WORDS - 1)));
        assert!(!worth_assessing("LGTM"));
        let mostly_code = format!("{}\n\n```\n{}\n```\n", words(10), words(100));
        assert!(!worth_assessing(&mostly_code), "code doesn't count");
    }
}
