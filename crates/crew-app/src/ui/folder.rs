//! Pieces shared by threads and reviews of folder projects: a folder of
//! repos with a copy per thread, or any folder worked on in place.

use crew_core::agent::PermissionMode;
use crew_core::event::RunStatus;
use crew_core::ids::{ProjectId, ThreadId};
use crew_core::project_kind::{ProjectKind, WorkMode};
use crew_core::store::StoredRun;
use gpui_kit::component::Icon;
use gpui_kit::*;

use crate::model::State as AppState;
use crate::theme::color;
use crate::theme::scale::{self, ui_px};

/// What the in-place callouts say.
pub const IN_PLACE_WARNING: &str = "Agents edit your files directly. There's no undo outside git.";

/// A repo, as a branch.
pub const BRANCH_ICON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><circle cx="4.5" cy="3.5" r="1.5"/><circle cx="4.5" cy="12.5" r="1.5"/><path d="M4.5 5v6"/></svg>"#;
/// A file outside git.
pub const FILE_ICON: &[u8] = br#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 16 16" fill="none" stroke="currentColor" stroke-width="1.5"><path d="M4 1.5h5.5L12.5 4.5v10h-8.5z"/></svg>"#;

/// How a thread of a folder project keeps its work. `None` is a single
/// repo with a worktree per thread, as before folder projects.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FolderMode {
    /// A thread folder with a worktree of each repo.
    OwnCopy,
    /// Agents write in the project folder itself.
    InPlace,
}

/// The folder mode of `thread`'s project, if it isn't a single repo with
/// a worktree per thread.
pub fn folder_mode(state: &AppState, thread: ThreadId) -> Option<FolderMode> {
    let project = state
        .thread_project(thread)
        .and_then(|p| state.project(p))?;
    let repos = state
        .thread(thread)
        .and_then(|t| t.worktree.as_ref())
        .is_some_and(|wt| !wt.repos.is_empty());
    match (&project.kind, project.work_mode) {
        (_, WorkMode::InPlace) => Some(FolderMode::InPlace),
        (ProjectKind::Repo, WorkMode::OwnCopy) if !repos => None,
        _ => Some(FolderMode::OwnCopy),
    }
}

/// "a", "a and b", "a, b and c".
pub fn and_list(items: &[String]) -> String {
    match items {
        [] => String::new(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

/// "1st", "2nd", "3rd", "4th", "11th", "21st".
pub fn ordinal(n: usize) -> String {
    let suffix = match (n % 10, n % 100) {
        (_, 11..=13) => "th",
        (1, _) => "st",
        (2, _) => "nd",
        (3, _) => "rd",
        _ => "th",
    };
    format!("{n}{suffix}")
}

/// "1 file", "2 files".
pub fn files_count(n: usize) -> String {
    format!("{n} {}", if n == 1 { "file" } else { "files" })
}

/// Runs writing in `project`'s folder, oldest first: agents that may
/// change files, working now.
pub fn writers(state: &AppState, project: ProjectId) -> Vec<&StoredRun> {
    state
        .running()
        .into_iter()
        .filter(|r| r.status == RunStatus::Working)
        .filter(|r| state.thread_project(r.thread) == Some(project))
        .filter(|r| {
            state
                .agent(r.agent)
                .is_some_and(|a| a.spec.permission_mode != PermissionMode::ReadOnly)
        })
        .collect()
}

/// Runs waiting for the folder of `project`, first in line first.
pub fn write_queue(state: &AppState, project: ProjectId) -> Vec<&StoredRun> {
    state
        .running()
        .into_iter()
        .filter(|r| r.status == RunStatus::Queued && r.waiting_for.is_some())
        .filter(|r| state.thread_project(r.thread) == Some(project))
        .collect()
}

/// "{Agent} will start when {Other} finishes writing.", for the first
/// run of `thread` that waits for another to finish writing.
pub fn queue_hint(state: &AppState, thread: ThreadId) -> Option<String> {
    let name = |run: &StoredRun| state.agent(run.agent).map(|a| a.spec.name.clone());
    state.live_runs(thread).into_iter().find_map(|run| {
        let other = state.run(run.waiting_for?)?;
        Some(format!(
            "{} will start when {} finishes writing.",
            name(run)?,
            name(other)?
        ))
    })
}

/// A mono chip as the boards draw them: no border, `fg` on `bg`.
pub fn mode_chip(
    id: impl Into<ElementId>,
    text: impl Into<SharedString>,
    fg: &str,
    bg: &str,
) -> Stateful<Div> {
    let text = text.into();
    div()
        .id(id)
        .flex_none()
        .px(ui_px(6.))
        .rounded(ui_px(4.))
        .bg(color(bg))
        .font_family(scale::code_family())
        .text_size(scale::mono(11.))
        .line_height(scale::mono_line(16.))
        .text_color(color(fg))
        .whitespace_nowrap()
        .aria_label(text.clone())
        .child(text)
}

/// "in place", in the warning colours.
pub fn in_place_chip(id: &'static str) -> impl IntoElement {
    mode_chip(id, "in place", "warning", "warning-subtle").test_support()
}

/// The warning callout: a triangle and `text`.
pub fn warning_note(id: &'static str, text: impl Into<SharedString>) -> impl IntoElement {
    let text = text.into();
    div()
        .id(id)
        .flex()
        .gap(ui_px(10.))
        .px(ui_px(12.))
        .py(scale::row(10.))
        .border_1()
        .border_color(color("warning-border"))
        .bg(color("warning-subtle"))
        .rounded(ui_px(6.))
        .text_size(scale::text(12.))
        .line_height(scale::line(18.))
        .aria_label(text.clone())
        .child(
            div().mt(ui_px(2.)).flex_none().child(
                Icon::new(gpui_kit::component::IconName::TriangleAlert)
                    .size(ui_px(14.))
                    .text_color(color("warning")),
            ),
        )
        .child(div().flex_1().min_w_0().child(text))
        .test_support()
}

/// A 12 px icon from inline SVG.
pub fn small_icon(svg: &'static [u8], token: &str) -> Icon {
    Icon::default()
        .data(svg)
        .size(ui_px(12.))
        .text_color(color(token))
}

#[cfg(test)]
mod tests {
    use super::{and_list, files_count, ordinal};

    #[test]
    fn lists_read_as_english() {
        assert_eq!(and_list(&[]), "");
        assert_eq!(and_list(&["api".into()]), "api");
        assert_eq!(and_list(&["api".into(), "web".into()]), "api and web");
        assert_eq!(
            and_list(&["api".into(), "web".into(), "infra".into()]),
            "api, web and infra"
        );
        assert_eq!(files_count(1), "1 file");
        assert_eq!(files_count(4), "4 files");
    }

    #[test]
    fn places_in_line_read_as_ordinals() {
        let got: Vec<String> = [1, 2, 3, 4, 11, 12, 13, 21, 22, 101]
            .into_iter()
            .map(ordinal)
            .collect();
        assert_eq!(
            got,
            [
                "1st", "2nd", "3rd", "4th", "11th", "12th", "13th", "21st", "22nd", "101st"
            ]
        );
    }
}
