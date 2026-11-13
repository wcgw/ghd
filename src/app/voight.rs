//! voight in ghd (the kit's D17): `:voight [heatmap|on|off]`, its statusline segment, the `<leader>h` map, and the
//! Heatmap over what a Preview or a Markdown file shows. The kit knows nothing of voight, and voight nothing of the
//! kit: voight's Assessor runs beside, its events arriving as [`Msg::Voight`](super::Msg::Voight).

use wcgw_kit::shell::{Command, Segment, Side};

use super::{Effect, Effects, Ghd, Io, Kit};
use crate::assessment::{self, Assessed, Readiness};
use crate::keymap::{Mode, VOIGHT};

/// What `:voight` takes.
const ARGUMENTS: [&str; 3] = ["heatmap", "on", "off"];

/// Adds `:voight`, its segment and its map to `kit`.
pub(super) fn install(kit: &mut Kit) {
    kit.add_command(
        Command::<Ghd>::new("voight", 2, |ghd, kit, line| ghd.voight(kit, line.args))
            .help("voight's Assessment; heatmap shows it on the text, on and off start and stop it")
            .complete(|_, _, before, _| {
                if before.is_empty() {
                    ARGUMENTS.map(str::to_owned).to_vec()
                } else {
                    Vec::new()
                }
            }),
    );
    kit.add_segment(Segment::new(Side::Right, 20, |ghd: &Ghd, _| {
        ghd.voight_label()
    }));
    kit.keymap_mut().bind(
        &[Mode::Normal],
        "<leader>h",
        "voight heatmap",
        "voight's Heatmap",
        VOIGHT,
    );
}

impl Ghd {
    /// `:voight`: says where voight stands; `heatmap` shows the Heatmap or stops; `on` and `off` start and stop
    /// assessing.
    fn voight(&mut self, kit: &mut Kit, args: &str) -> Result<Effects, String> {
        match args {
            "" => {
                kit.info(
                    self.voight_label()
                        .unwrap_or_else(|| self.voight_off().to_owned()),
                );
            }
            "heatmap" => {
                if self.assessments.is_none() {
                    kit.info(format!("No Heatmap: {}", self.voight_off()));
                    return Ok(Vec::new());
                }
                self.heatmap = !self.heatmap;
                self.heat_version += 1;
                if self.heatmap && self.assessment_on_screen().is_none() {
                    kit.info("The Heatmap shows on text voight has assessed");
                }
            }
            "on" if !self.voight_built => return Err(self.voight_off().to_owned()),
            "on" => {
                if self.assessments.is_none() {
                    self.assessments = Some(assessment::Assessments::default());
                    // Its model is loaded already, or still loading, which it says when done.
                    if let Some(assessments) = &mut self.assessments {
                        assessments.loaded(self.voight_ready);
                    }
                }
            }
            "off" => {
                self.assessments = None;
                self.heatmap = false;
                self.heat_version += 1;
            }
            args => return Err(format!("E475: Invalid argument: {args}")),
        }
        Ok(Vec::new())
    }

    /// Why there's no voight.
    fn voight_off(&self) -> &'static str {
        if !cfg!(feature = "voight") {
            "voight is off: this ghd was built without it"
        } else if self.voight_built {
            "voight is off: :voight on starts it again"
        } else {
            "voight is off: voight = \"off\" in config.toml"
        }
    }

    /// What voight says of what's on screen, for the statusline: that it's loading, assessing, its Assessment, or
    /// that it failed; `None` when there's nothing to say, as for a text with too little prose.
    pub(super) fn voight_label(&self) -> Option<String> {
        let assessments = self.assessments.as_ref()?;
        let key = self.assessable.as_ref();
        match assessments.readiness() {
            Readiness::Loading if key.is_some() || !self.files.is_open() => {
                return Some("voight loading…".to_owned());
            }
            Readiness::Loading | Readiness::Failed => return None,
            Readiness::Ready => {}
        }
        match assessments.get(key?) {
            None | Some(Assessed::Assessing) => Some("voight …".to_owned()),
            Some(Assessed::Done(Some(assessment))) => {
                Some(format!("voight {:.0}%", assessment.share * 100.0))
            }
            Some(Assessed::Done(None)) => None,
            Some(Assessed::Failed(_)) => Some("voight failed".to_owned()),
        }
    }

    /// The Assessment of the text on screen, once made.
    fn assessment_on_screen(&self) -> Option<&assessment::Assessment> {
        match self.assessments.as_ref()?.get(self.assessable.as_ref()?)? {
            Assessed::Done(Some(assessment)) => Some(assessment),
            _ => None,
        }
    }

    /// Takes in what voight says: that its model is loaded, or why it couldn't be, or an Assessment.
    pub(super) fn voight_said(&mut self, kit: &mut Kit, event: assessment::Event) {
        if let assessment::Event::Loaded { .. } = event {
            self.voight_ready = true;
        }
        let Some(assessments) = &mut self.assessments else {
            return;
        };
        match event {
            assessment::Event::Loaded { fallback } => {
                assessments.loaded(true);
                if let Some(fallback) = fallback {
                    kit.info(fallback);
                }
            }
            assessment::Event::Failed(why) => {
                assessments.loaded(false);
                kit.error(format!("voight couldn't load its model: {why}"));
            }
            assessment::Event::Assessed { key, assessment } => {
                assessments.assessed(key, assessment);
                self.heat_version += 1;
            }
        }
    }

    /// Notes what on screen voight would assess, and asks for its Assessment once voight is ready and assessing
    /// nothing else.
    pub(super) fn assess(&mut self, kit: &mut Kit) -> Effects {
        if self.assessments.is_none() {
            self.assessable = None;
            return Vec::new();
        }
        let assessable = if self.files.is_open() {
            self.file_assessable(kit)
        } else {
            self.preview_assessable(kit)
        };
        self.assessable = assessable.as_ref().map(|(key, _)| key.clone());
        let Some((key, text)) = assessable else {
            return Vec::new();
        };
        let Some(assessments) = &mut self.assessments else {
            return Vec::new();
        };
        if !assessments.can_assess() || assessments.get(&key).is_some() {
            return Vec::new();
        }
        assessments.assessing(key.clone());
        vec![Effect::App(Io::Assess { key, text })]
    }
}
