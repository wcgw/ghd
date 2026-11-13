//! ghd's ex commands (the kit's D13): every action on Notifications has one, so `:map` can reach anything a key
//! does, and `:help` lists them.

use wcgw_kit::shell::Command;

use super::{Effect, Effects, Ghd, Kit, Target};
use crate::filter::{self, Filter};
use crate::inbox::ThreadId;

/// Adds ghd's commands to `kit`, with the shortest abbreviation Vim would accept for each.
pub(super) fn add(kit: &mut Kit) {
    let commands = [
        Command::<Ghd>::new("filter", 3, |ghd, kit, line| ghd.narrow(kit, line.args))
            .help("narrow the View on screen to what a filter matches; alone, all again")
            .complete(complete_filter),
        Command::<Ghd>::new("select", 3, |ghd, kit, line| {
            ghd.select(kit, line.args, true)
        })
        .help("select what a filter matches in the View; alone, all it shows")
        .complete(complete_filter),
        Command::<Ghd>::new("unselect", 3, |ghd, kit, line| {
            ghd.select(kit, line.args, false)
        })
        .help("unselect what a filter matches in the View; alone, all it shows")
        .complete(complete_filter),
        Command::<Ghd>::new("toggle", 3, |ghd, kit, line| {
            if line.bang {
                ghd.toggle_all(kit);
            } else {
                ghd.toggle(kit);
            }
            Ok(Vec::new())
        })
        .bang()
        .help("select the Notification at the cursor, or not; with !, all shown, or none"),
        Command::<Ghd>::new("done", 4, |ghd, kit, _| {
            let effects = ghd.mark_done(kit);
            ghd.render_views(kit);
            Ok(effects)
        })
        .help("mark Done, or in the Done list, bring back to ghd's Inbox"),
        Command::<Ghd>::new("read", 1, |ghd, kit, _| {
            let targets = ghd.take_targets(kit);
            let effects = ghd.mark_read(targets);
            if effects.len() > 1 {
                kit.info(format!("Marked {} read", effects.len()));
            }
            ghd.render_views(kit);
            Ok(effects)
        })
        .help("mark read"),
        Command::<Ghd>::new("donelist", 5, |ghd, kit, _| {
            // The Selection is of the list that was on screen.
            ghd.selection.clear();
            ghd.showing_done = !ghd.showing_done;
            ghd.render_views(kit);
            Ok(Vec::new())
        })
        .help("show the Done list in every View, or the Inbox again"),
        Command::<Ghd>::new("unreadonly", 3, |ghd, kit, _| {
            if ghd.showing_done {
                kit.info("Unread only is for the Inbox, not the Done list");
                return Ok(Vec::new());
            }
            let shown = &mut ghd.views[ghd.current];
            shown.unread_only = !shown.unread_only;
            ghd.render_views(kit);
            Ok(Vec::new())
        })
        .help("show only the unread in the View on screen, or all again"),
        Command::<Ghd>::new("preview", 4, |ghd, kit, _| ghd.toggle_preview(kit))
            .help("show the Preview below the list, or hide it"),
        Command::<Ghd>::new("files", 3, |ghd, kit, _| {
            ghd.toggle_files(kit);
            Ok(Vec::new())
        })
        .help("the pull request's Changed files, or the list again"),
        Command::<Ghd>::new("whole", 3, |ghd, _, _| ghd.toggle_whole())
            .help("on the Files screen, files whole, or as their diffs again"),
        Command::<Ghd>::new("browse", 3, |ghd, kit, line| match line.args {
            "" => Ok(ghd.browse_subject(kit)),
            "link" => match kit.target_under_cursor() {
                Some(Target::Url(url)) => Ok(vec![Effect::OpenUrl(url)]),
                _ => Err("No link under the cursor".to_owned()),
            },
            args => Err(format!("E475: Invalid argument: {args}")),
        })
        .help("open on GitHub, which marks read; with link, the link under the cursor")
        .complete(|_, _, before, _| {
            if before.is_empty() {
                vec!["link".to_owned()]
            } else {
                Vec::new()
            }
        }),
        Command::<Ghd>::new("yanklink", 5, |ghd, kit, _| {
            let url = ghd
                .subject_url(kit)
                .ok_or_else(|| "No Notification under the cursor".to_owned())?;
            kit.info(format!("Yanked {url}"));
            Ok(vec![Effect::Copy(url)])
        })
        .help("yank the link :browse opens"),
        Command::<Ghd>::new("clear", 3, |ghd, kit, _| {
            ghd.clear(kit);
            Ok(Vec::new())
        })
        .help("clear the Selection, or else the filter; on the Files screen, back to the list"),
    ];
    for command in commands {
        kit.add_command(command);
    }
}

/// What `:filter`, `:select` and `:unselect` complete the word typed to: a qualifier, or once it has one, its values
/// that would match some of the Notifications the View on screen shows.
fn complete_filter(ghd: &Ghd, _: &Kit, _: &[&str], word: &str) -> Vec<String> {
    filter::completions(word, &ghd.shown_with_states())
}

impl Ghd {
    /// `:filter {filter}`: narrows whichever View is on screen to what `typed` matches; all of it again when empty.
    fn narrow(&mut self, kit: &mut Kit, typed: &str) -> Result<Effects, String> {
        self.narrowing = if typed.is_empty() {
            None
        } else {
            let filter: Filter = typed.parse().map_err(|error: filter::FilterError| {
                format!("Not a filter: {}", error.problem())
            })?;
            Some((typed.to_owned(), filter))
        };
        self.render_views(kit);
        Ok(Vec::new())
    }

    /// `:select {filter}` (`selecting`) and `:unselect {filter}`: what `typed` matches in the View on screen, all it
    /// shows when empty, says how many it changed: some may have been selected, or not, already.
    fn select(&mut self, kit: &mut Kit, typed: &str, selecting: bool) -> Result<Effects, String> {
        if self.files.is_open() {
            return Err("Selecting is for the list, not the Files screen".to_owned());
        }
        let filter: Filter = typed
            .parse()
            .map_err(|error: filter::FilterError| format!("Not a filter: {}", error.problem()))?;
        let matching: Vec<ThreadId> = self
            .shown_in(self.current)
            .filter(|n| filter.matches(n, self.states.get(n)))
            .map(|n| n.thread)
            .collect();
        let before = self.selection.len();
        if selecting {
            self.selection.extend(matching);
            let selected = self.selection.len() - before;
            kit.info(format!(
                "Selected {}",
                super::counted(selected as u64, "Notification")
            ));
        } else {
            for thread in &matching {
                self.selection.remove(thread);
            }
            let unselected = before - self.selection.len();
            kit.info(format!(
                "Unselected {}",
                super::counted(unselected as u64, "Notification")
            ));
        }
        self.render_views(kit);
        Ok(Vec::new())
    }

    /// `:toggle` (`s`): selects the Notification at the cursor, or unselects it.
    fn toggle(&mut self, kit: &mut Kit) {
        if let Some(thread) = self.at_cursor(kit) {
            if !self.selection.remove(&thread) {
                self.selection.insert(thread);
            }
            self.render_views(kit);
        }
    }

    /// `:toggle!` (`A`): selects every Notification the View on screen shows, or unselects them if they're all
    /// selected.
    fn toggle_all(&mut self, kit: &mut Kit) {
        let shown: Vec<ThreadId> = self.shown_in(self.current).map(|n| n.thread).collect();
        if !shown.is_empty() && shown.iter().all(|thread| self.selection.contains(thread)) {
            for thread in &shown {
                self.selection.remove(thread);
            }
        } else {
            self.selection.extend(shown);
        }
        self.render_views(kit);
    }

    /// `:clear` (`<Esc>`): clears the Selection, or else the filter; on the Files screen, goes back to the list.
    fn clear(&mut self, kit: &mut Kit) {
        if self.files.is_open() {
            self.leave_files(kit);
        } else if !self.selection.is_empty() {
            self.selection.clear();
        } else {
            self.narrowing = None;
        }
        self.render_views(kit);
    }

    /// `:browse` (`gX`): the Subjects acted on in the browser, which marks them read; on the Files screen, the
    /// Changed file at the cursor.
    fn browse_subject(&mut self, kit: &mut Kit) -> Effects {
        if self.files.is_open() {
            let Some((url, thread)) = self.selected_file_url(kit) else {
                return Vec::new();
            };
            let mut effects = vec![Effect::OpenUrl(url)];
            effects.extend(self.mark_read(vec![thread]));
            self.render_views(kit);
            return effects;
        }
        let targets = self.take_targets(kit);
        self.browse(kit, targets)
    }

    /// Where the Subject at the cursor is on GitHub; on the Files screen, the Changed file at the cursor.
    fn subject_url(&self, kit: &mut Kit) -> Option<String> {
        if self.files.is_open() {
            return self.selected_file_url(kit).map(|(url, _)| url);
        }
        let thread = self.at_cursor(kit)?;
        Some(self.notification(thread)?.subject_url.clone())
    }
}
