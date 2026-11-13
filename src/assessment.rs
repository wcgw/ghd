//! Assessments: voight's estimate of how much of the text a Preview shows was written by an LLM (see
//! `CONTEXT.md`). Never evidence of authorship.
//!
//! voight's `Assessor` does the work, on a thread of its own (see [`start`]); what ghd keeps here is what its
//! interface needs: which text an Assessment is of, and those made so far. The types voight speaks in are mirrored
//! here, so that ghd's interface builds, and is tested, without voight too.

use std::collections::HashMap;
use std::fmt;
use std::ops::Range;

use chrono::{DateTime, Utc};

use crate::inbox::{Listing, ThreadId};
use crate::preview::Place;

/// Where voight runs its model.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Device {
    Cpu,
    /// A Mac's GPU.
    Metal,
    /// An NVIDIA GPU.
    Cuda,
    /// Any other GPU, through wgpu: Vulkan on Linux.
    Wgpu,
}

impl Device {
    /// Whether this ghd was built to run voight there: CUDA with its `cuda` feature, wgpu with its `wgpu` one, and
    /// Metal with its `metal` one or on Apple silicon, where voight builds with it unless with wgpu, whose fork of
    /// candle doesn't. Whether the machine has one is another matter.
    pub fn is_built_in(self) -> bool {
        match self {
            Self::Cpu => cfg!(feature = "voight"),
            Self::Metal => cfg!(all(
                feature = "voight",
                any(
                    feature = "metal",
                    all(
                        target_os = "macos",
                        target_arch = "aarch64",
                        not(feature = "wgpu")
                    )
                )
            )),
            Self::Cuda => cfg!(feature = "cuda"),
            Self::Wgpu => cfg!(feature = "wgpu"),
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
            Self::Wgpu => "wgpu",
        })
    }
}

/// What an Assessment is of: one of a thread's texts, with the activity its Notification had.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Key {
    pub thread: ThreadId,
    pub as_of: DateTime<Utc>,
    pub part: Part,
}

/// Which of a thread's texts an Assessment is of.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Part {
    /// What its Preview shows at a place: the Subject's body, or one of its comments.
    Preview(Place),
    /// One of its Changed files, by its contents path.
    File(String),
}

/// What voight has to say.
#[derive(Debug, Clone, PartialEq)]
#[cfg_attr(
    not(feature = "voight"),
    allow(
        dead_code,
        reason = "built without voight, only the tests say anything"
    )
)]
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

/// Starts voight's Assessor: fetches its model (downloading it the first time, about 1.7 GB, showing its progress
/// on stderr: call it before the terminal UI takes the screen), then loads it on `device` on a thread of its own.
/// `on_event` hears how loading went and every Assessment, on that thread.
///
/// # Errors
///
/// Fails if the model can't be fetched, or the thread started.
#[cfg(feature = "voight")]
pub fn start(
    device: Device,
    on_event: impl Fn(Event) + Send + 'static,
) -> anyhow::Result<voight::assessor::Assessor<Key>> {
    use voight::assessor::{self, Processor};

    let processor = match device {
        Device::Cpu => Processor::Cpu,
        Device::Metal => Processor::Metal,
        Device::Cuda => Processor::Cuda,
        Device::Wgpu => Processor::Wgpu,
    };
    assessor::Assessor::start(voight::Model::Gradient, processor, move |event| {
        on_event(match event {
            assessor::Event::Loaded { fallback } => Event::Loaded { fallback },
            assessor::Event::Failed(why) => Event::Failed(why),
            assessor::Event::Assessed { key, assessment } => Event::Assessed {
                key,
                assessment: assessment.map(|made| {
                    made.map(|made| Assessment {
                        share: made.share,
                        windows: made
                            .windows
                            .into_iter()
                            .map(|window| Window {
                                range: window.range,
                                share: window.share,
                            })
                            .collect(),
                    })
                }),
            },
        });
    })
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
            kept.thread != key.thread || kept.part != key.part || *assessed == Assessed::Assessing
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
            part: Part::Preview(Place::Body),
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
    fn each_text_keeps_its_assessment_until_newer_activity() {
        let mut assessments = Assessments::default();
        let comment = |at: usize| Key {
            part: Part::Preview(Place::Comment(at)),
            ..key(1)
        };
        assessments.assessed(key(1), Ok(None));
        assessments.assessed(comment(0), Ok(None));

        assessments.assessing(comment(1));
        assert!(assessments.get(&key(1)).is_some(), "the body's");
        assert!(assessments.get(&comment(0)).is_some(), "another comment's");

        assessments.assessing(Key {
            as_of: key(1).as_of + TimeDelta::minutes(5),
            ..comment(0)
        });
        assert_eq!(assessments.get(&comment(0)), None, "for older activity");
        assert!(assessments.get(&key(1)).is_some());
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
}
