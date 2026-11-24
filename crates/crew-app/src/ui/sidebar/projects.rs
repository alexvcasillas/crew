//! The projects part of the sidebar: how projects are grouped and
//! ordered, the hint that tells same-named projects apart, a project's
//! row and its menu.

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::{Component, Path};

use crew_core::ids::{ChannelId, ProjectId};
use crew_proto::Request;
use gpui_kit::component::menu::{ContextMenuExt as _, PopupMenu, PopupMenuItem};
use gpui_kit::component::{Icon, IconName};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::common::with_font_probe;
use super::super::palette::highlighted;
use super::super::settings::Page;
use super::super::workspace::{Place, Workspace};
use super::{ChannelBadge, render_badge, sidebar_row, status_dot};
use crate::prefs::{ShowProjects, SortProjects};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// A project with activity this recent counts as active.
pub(crate) const ACTIVE_WITHIN_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// What grouping needs to know about a project.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ProjectInfo {
    pub(crate) id: ProjectId,
    pub(crate) name: String,
    pub(crate) pinned: bool,
    /// A run works or waits for a slot, or something needs you.
    pub(crate) live: bool,
    /// When something last happened in one of its threads, in Unix ms.
    pub(crate) activity: Option<i64>,
    /// When it was added, for ordering projects without activity.
    pub(crate) created_at: i64,
}

impl ProjectInfo {
    /// Live, or a thread did something in the last 7 days.
    pub(crate) fn is_active(&self, now: i64) -> bool {
        self.live || self.activity.is_some_and(|at| now - at <= ACTIVE_WITHIN_MS)
    }
}

/// The sidebar's project groups, each in the chosen order.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Groups {
    pub(crate) pinned: Vec<ProjectId>,
    pub(crate) active: Vec<ProjectId>,
    pub(crate) other: Vec<ProjectId>,
}

/// Puts each project in Pinned, Active (live, or thread activity in
/// the last 7 days) or Other, ordered by `sort`, and keeps only what
/// `show` asks for, except `keep` (the open channel's project), which
/// always stays so the selection shows.
pub(crate) fn group_projects(
    mut projects: Vec<ProjectInfo>,
    show: ShowProjects,
    sort: SortProjects,
    now: i64,
    keep: Option<ProjectId>,
) -> Groups {
    sort_projects(&mut projects, sort);
    let mut groups = Groups::default();
    for p in projects {
        let active = p.is_active(now);
        let shown = keep == Some(p.id)
            || match show {
                ShowProjects::All => true,
                ShowProjects::Active => p.pinned || active,
                ShowProjects::Pinned => p.pinned,
            };
        if !shown {
            continue;
        }
        if p.pinned {
            groups.pinned.push(p.id);
        } else if active {
            groups.active.push(p.id);
        } else {
            groups.other.push(p.id);
        }
    }
    groups
}

/// Most recent activity first, or by name; ties by name.
pub(crate) fn sort_projects(projects: &mut [ProjectInfo], sort: SortProjects) {
    let name = |p: &ProjectInfo| (p.name.to_lowercase(), p.id.0);
    match sort {
        SortProjects::Recent => {
            let at = |p: &ProjectInfo| p.activity.unwrap_or(p.created_at);
            projects.sort_by(|a, b| at(b).cmp(&at(a)).then(name(a).cmp(&name(b))));
        }
        SortProjects::Name => projects.sort_by_key(name),
    }
}

/// Folder names a hint keeps at least, as "~/code/a" has.
const HINT_KEEPS: usize = 3;

/// For projects whose names clash (ignoring case), the folder each one
/// sits in, with `~` for `home`: "~/code/a". A path longer than three
/// folders loses folders from the left ("…/acme/src/a") only while the
/// hints still tell the projects apart. Projects in the same folder show their own path
/// instead. Projects whose names don't clash get no hint.
pub(crate) fn duplicate_hints(
    projects: &[(ProjectId, &str, &Path)],
    home: Option<&Path>,
) -> HashMap<ProjectId, String> {
    let mut by_name: HashMap<String, Vec<(ProjectId, &Path)>> = HashMap::new();
    for (id, name, root) in projects {
        by_name
            .entry(name.to_lowercase())
            .or_default()
            .push((*id, root));
    }
    let mut hints = HashMap::new();
    for clash in by_name.into_values().filter(|c| c.len() > 1) {
        let parents: Vec<&Path> = clash
            .iter()
            .map(|(_, root)| root.parent().unwrap_or(root))
            .collect();
        let shared = parents.windows(2).all(|w| w[0] == w[1]);
        let parts: Vec<(Vec<OsString>, bool)> = clash
            .iter()
            .zip(&parents)
            .map(|((_, root), parent)| path_parts(if shared { root } else { parent }, home))
            .collect();
        // Drop folders from the left while the hints stay apart.
        let mut dropped = 0;
        loop {
            let next = dropped + 1;
            let fits = parts.iter().all(|(p, _)| p.len() >= next + HINT_KEEPS);
            if !fits || !distinct(&hint_texts(&parts, next)) {
                break;
            }
            dropped = next;
        }
        for ((id, _), hint) in clash.iter().zip(hint_texts(&parts, dropped)) {
            hints.insert(*id, hint);
        }
    }
    hints
}

/// Each path less its first `dropped` folders, "…/" marking the cut.
fn hint_texts(parts: &[(Vec<OsString>, bool)], dropped: usize) -> Vec<String> {
    parts
        .iter()
        .map(|(p, absolute)| {
            let joined = p[dropped..]
                .iter()
                .map(|s| s.to_string_lossy())
                .collect::<Vec<_>>()
                .join("/");
            if dropped > 0 {
                format!("…/{joined}")
            } else if *absolute {
                format!("/{joined}")
            } else {
                joined
            }
        })
        .collect()
}

fn distinct(texts: &[String]) -> bool {
    let mut seen: Vec<&String> = texts.iter().collect();
    seen.sort();
    seen.dedup();
    seen.len() == texts.len()
}

/// `root`'s folder names, with the home folder as one `~`, and whether
/// the path starts at `/`.
fn path_parts(root: &Path, home: Option<&Path>) -> (Vec<OsString>, bool) {
    if let Some(rest) = home.and_then(|h| root.strip_prefix(h).ok()) {
        let mut parts = vec![OsString::from("~")];
        parts.extend(names(rest));
        return (parts, false);
    }
    (names(root), root.has_root())
}

fn names(path: &Path) -> Vec<OsString> {
    path.components()
        .filter_map(|c| match c {
            Component::Normal(name) => Some(name.to_os_string()),
            _ => None,
        })
        .collect()
}

/// How a project row is drawn.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct ProjectLook {
    pub(crate) project: ProjectId,
    /// Its one channel, when the row stands for it.
    pub(crate) single: Option<ChannelId>,
    pub(crate) open: bool,
    /// The filter result it is, if the filter is on.
    pub(crate) hit: Option<usize>,
}

impl Workspace {
    /// A project's row: a chevron (or room for one when it has a single
    /// channel), its name, the hint for a clashing name, and its status.
    /// It opens and closes, or opens its one channel.
    pub(super) fn render_project(&self, look: ProjectLook, cx: &mut Context<Self>) -> AnyElement {
        let state = self.state();
        let Some(project) = state.project(look.project) else {
            return div().into_any_element();
        };
        let id = look.project;
        let name = project.name.clone();
        let query = self.sidebar.query.trim().to_string();
        let range = super::super::palette::match_range(&name, &query);
        let matched = range.clone().map(|r| format!("Matches “{}”", &name[r]));
        let hint = self.sidebar.hints.get(&id).cloned();
        #[cfg(test)]
        if let Some(channel) = look.single {
            super::row_probe::single(channel, id);
        }
        let selected = match look.single {
            Some(channel) => self.nav.channel == Some(channel),
            None => false,
        } || look.hit.is_some_and(|hit| hit == self.sidebar.selected);
        let badge = match look.single {
            Some(channel) => self.channel_badge(channel),
            // Open, its channels carry their own dots.
            None if look.open => ChannelBadge::None,
            None => match status_dot(state.project_status(id)) {
                Some(_) => ChannelBadge::Status(state.project_status(id)),
                None => ChannelBadge::None,
            },
        };
        let lead = if look.single.is_some() {
            div().flex_none().w(ui_px(12.)).into_any_element()
        } else {
            Icon::new(if look.open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            })
            .size(ui_px(12.))
            .text_color(color("text-secondary"))
            .into_any_element()
        };
        let this = cx.entity().downgrade();
        let pinned = project.pinned_at.is_some();
        let first = state.channels(id).first().map(|c| c.name.clone());
        sidebar_row(format!("project-{id}"))
            .gap(ui_px(8.))
            .when(selected, |row| row.bg(color("surface-300")))
            .text_color(color("text-primary"))
            .aria_label(name.clone())
            .aria_selected(selected)
            .child(lead)
            .child(
                div()
                    .id(SharedString::from(format!("project-name-{id}")))
                    .min_w_0()
                    .truncate()
                    .when(hint.is_none(), |name| name.flex_1())
                    .font_weight(weight(FontWeight::MEDIUM))
                    .when_some(matched, |name, matched| name.aria_description(matched))
                    .child(highlighted(&name, range))
                    .map(|label| {
                        // Tests read it as the label of the channel it opens.
                        with_font_probe(label, || match look.single {
                            Some(channel) => format!("channel-label-{channel}"),
                            None => format!("project-label-{id}"),
                        })
                    })
                    .test_support(),
            )
            .children(hint.map(|hint| {
                let text = format!("· {hint}");
                div()
                    .id(SharedString::from(format!("project-hint-{id}")))
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .text_color(color("text-tertiary"))
                    .aria_label(text.clone())
                    .child(text)
                    .test_support()
            }))
            .child(render_badge(format!("project-status-{id}"), &badge))
            .on_click(cx.listener(move |this, _, window, cx| match look.single {
                Some(channel) => this.select_channel(channel, window, cx),
                None => this.toggle_project(id, cx),
            }))
            .test_support()
            .context_menu(move |menu, _, _| project_menu(menu, this.clone(), id, pinned, &first))
            .into_any_element()
    }

    /// Pins the project to the top of the sidebar, or unpins it.
    pub(super) fn pin_project(
        &mut self,
        project: ProjectId,
        pinned: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.request(Request::PinProject { project, pinned }, window, cx);
    }

    /// Starts a new thread in the project's first channel.
    fn new_thread_in_project(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(channel) = self.state().channels(project).first().map(|c| c.id) else {
            return;
        };
        self.leave_pages(cx);
        self.select_channel(channel, window, cx);
        self.new_thread(window, cx);
    }

    /// Runs `open` with `args` off the UI thread, showing `failed` if it
    /// doesn't work.
    fn run_open(
        &mut self,
        args: Vec<OsString>,
        failed: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let opener = self.opener.clone();
        let opened = cx.background_executor().spawn(async move { opener(args) });
        self.spawn_request(window, cx, async move |_, _| {
            opened.await.map_err(|e| {
                eprintln!("crew: open failed: {e}");
                (failed, None)
            })
        });
    }

    fn reveal_project(&mut self, project: ProjectId, window: &mut Window, cx: &mut Context<Self>) {
        let Some(root) = self.state().project(project).map(|p| p.root.clone()) else {
            return;
        };
        let args = vec!["-R".into(), root.into_os_string()];
        let failed = "Finder couldn't show the folder. Check that it still exists.".to_string();
        self.run_open(args, failed, window, cx);
    }

    fn open_project_in_terminal(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(root) = self.state().project(project).map(|p| p.root.clone()) else {
            return;
        };
        let args = vec!["-a".into(), "Terminal".into(), root.into_os_string()];
        let failed = "Terminal didn't open. Check that the folder still exists.".to_string();
        self.run_open(args, failed, window, cx);
    }

    /// Settings › Projects on this project, where its setup files are.
    fn open_setup_files(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_settings(Page::Projects, window, cx);
        self.select_project(project, window, cx);
    }
}

/// A menu row with its shortcut on the right, labelled for tests.
fn menu_row(label: String, keys: Option<&'static str>, danger: bool) -> PopupMenuItem {
    PopupMenuItem::element(move |_, _| {
        div()
            .id(SharedString::from(format!("menu-{label}")))
            .flex()
            .items_center()
            .w_full()
            .gap(ui_px(8.))
            .aria_label(label.clone())
            .when(danger, |row| row.text_color(color("danger")))
            .child(div().flex_1().child(label.clone()))
            .children(keys.map(|keys| {
                div()
                    .font_family(code_family())
                    .text_size(scale::mono(11.))
                    .text_color(color("text-tertiary"))
                    .child(keys)
            }))
            .test_support()
    })
}

/// The project's right-click menu, as the board has it.
fn project_menu(
    menu: PopupMenu,
    this: WeakEntity<Workspace>,
    project: ProjectId,
    pinned: bool,
    first: &Option<String>,
) -> PopupMenu {
    let act = |f: fn(&mut Workspace, ProjectId, &mut Window, &mut Context<Workspace>)| {
        let this = this.clone();
        move |_: &ClickEvent, window: &mut Window, cx: &mut App| {
            this.update(cx, |this, cx| f(this, project, window, cx))
                .ok();
        }
    };
    let mut menu = menu
        .item(
            PopupMenuItem::new("Project overview").on_click(act(|this, p, window, cx| {
                this.open_place(Place::Project(p), window, cx)
            })),
        )
        .item(if pinned {
            PopupMenuItem::new("Unpin").on_click(act(|this, p, window, cx| {
                this.pin_project(p, false, window, cx)
            }))
        } else {
            PopupMenuItem::new("Pin to top").on_click(act(|this, p, window, cx| {
                this.pin_project(p, true, window, cx)
            }))
        });
    if let Some(first) = first {
        menu = menu.item(
            menu_row(format!("New thread in #{first}"), Some("⌘N"), false).on_click(act(
                |this, p, window, cx| this.new_thread_in_project(p, window, cx),
            )),
        );
    }
    menu.item(
        PopupMenuItem::new("New channel…").on_click(act(|this, p, window, cx| {
            this.open_new_channel_dialog(p, window, cx)
        })),
    )
    .separator()
    .item(
        PopupMenuItem::new("Reveal in Finder").on_click(act(|this, p, window, cx| {
            this.reveal_project(p, window, cx)
        })),
    )
    .item(
        PopupMenuItem::new("Open in terminal").on_click(act(|this, p, window, cx| {
            this.open_project_in_terminal(p, window, cx)
        })),
    )
    .item(
        PopupMenuItem::new("Copy path").on_click(act(|this, p, _, cx| {
            if let Some(root) = this.state().project(p).map(|p| p.root.clone()) {
                cx.write_to_clipboard(ClipboardItem::new_string(
                    root.to_string_lossy().into_owned(),
                ));
            }
        })),
    )
    .item(
        PopupMenuItem::new("Setup files…").on_click(act(|this, p, window, cx| {
            this.open_setup_files(p, window, cx)
        })),
    )
    .separator()
    .item(
        PopupMenuItem::new("Rename…").on_click(act(|this, p, window, cx| {
            this.open_rename_project_dialog(p, window, cx)
        })),
    )
    .item(
        menu_row("Remove project…".into(), None, true).on_click(act(|this, p, window, cx| {
            this.open_remove_project_dialog(p, window, cx)
        })),
    )
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use crew_core::ids::ProjectId;

    use super::{ACTIVE_WITHIN_MS, Groups, ProjectInfo, duplicate_hints, group_projects};
    use crate::prefs::{ShowProjects, SortProjects};

    const NOW: i64 = 100 * ACTIVE_WITHIN_MS;
    const DAY: i64 = 24 * 60 * 60 * 1000;

    fn info(name: &str, pinned: bool, live: bool, days_ago: Option<i64>) -> ProjectInfo {
        ProjectInfo {
            id: ProjectId::new(),
            name: name.into(),
            pinned,
            live,
            activity: days_ago.map(|d| NOW - d * DAY),
            created_at: NOW - 60 * DAY,
        }
    }

    #[test]
    fn groups_pinned_then_active_then_the_rest() {
        let pinned = info("curl-runner", true, false, Some(30));
        let live = info("api", false, true, Some(30));
        let recent = info("web", false, false, Some(2));
        let week = info("docs", false, false, Some(7));
        let old = info("legacy", false, false, Some(8));
        let all = vec![
            old.clone(),
            recent.clone(),
            live.clone(),
            pinned.clone(),
            week.clone(),
        ];
        let group = |show, sort, keep| group_projects(all.clone(), show, sort, NOW, keep);
        assert_eq!(
            group(ShowProjects::All, SortProjects::Recent, None),
            Groups {
                pinned: vec![pinned.id],
                active: vec![recent.id, week.id, live.id],
                other: vec![old.id],
            }
        );
        let by_name = group(ShowProjects::All, SortProjects::Name, None);
        assert_eq!(by_name.active, vec![live.id, week.id, recent.id]);

        let active = group(ShowProjects::Active, SortProjects::Name, None);
        assert_eq!(active.pinned, vec![pinned.id]);
        assert_eq!(active.active.len(), 3);
        assert!(active.other.is_empty());

        let only_pinned = group(ShowProjects::Pinned, SortProjects::Name, None);
        assert_eq!(only_pinned.pinned, vec![pinned.id]);
        assert!(only_pinned.active.is_empty() && only_pinned.other.is_empty());

        // The open channel's project stays, in its own group.
        let kept = group(ShowProjects::Pinned, SortProjects::Name, Some(old.id));
        assert_eq!(kept.other, vec![old.id]);
        assert!(kept.active.is_empty());
    }

    #[test]
    fn a_project_without_threads_is_never_active() {
        let quiet = ProjectInfo {
            created_at: NOW,
            ..info("new", false, false, None)
        };
        let busy = info("busy", false, false, Some(1));
        let groups = group_projects(
            vec![quiet.clone(), busy.clone()],
            ShowProjects::All,
            SortProjects::Recent,
            NOW,
            None,
        );
        assert_eq!(groups.active, vec![busy.id]);
        assert_eq!(groups.other, vec![quiet.id]);
        // Live always counts.
        let live = info("live", false, true, None);
        let groups = group_projects(
            vec![live.clone()],
            ShowProjects::All,
            SortProjects::Recent,
            NOW,
            None,
        );
        assert_eq!(groups.active, vec![live.id]);
    }

    #[test]
    fn names_sort_ignoring_case() {
        let a = info("Zeta", false, false, Some(1));
        let b = info("alpha", false, false, Some(1));
        let groups = group_projects(
            vec![a.clone(), b.clone()],
            ShowProjects::All,
            SortProjects::Name,
            NOW,
            None,
        );
        assert_eq!(groups.active, vec![b.id, a.id]);
    }

    fn hints(projects: &[(&str, &str)], home: Option<&str>) -> Vec<Option<String>> {
        let ids: Vec<ProjectId> = projects.iter().map(|_| ProjectId::new()).collect();
        let list: Vec<(ProjectId, &str, &Path)> = projects
            .iter()
            .zip(&ids)
            .map(|((name, root), id)| (*id, *name, Path::new(*root)))
            .collect();
        let got = duplicate_hints(&list, home.map(Path::new));
        ids.iter().map(|id| got.get(id).cloned()).collect()
    }

    #[test]
    fn only_clashing_names_get_their_folder_with_a_tilde_for_home() {
        assert_eq!(
            hints(
                &[
                    ("demo", "/Users/me/code/a/demo"),
                    ("demo", "/Users/me/code/b/demo"),
                    ("web", "/Users/me/code/web"),
                ],
                Some("/Users/me"),
            ),
            [Some("~/code/a".into()), Some("~/code/b".into()), None]
        );
        // Names clash ignoring case; outside home the path is whole.
        assert_eq!(
            hints(
                &[("Demo", "/work/one/demo"), ("demo", "/work/two/demo")],
                None
            ),
            [Some("/work/one".into()), Some("/work/two".into())]
        );
        assert_eq!(
            hints(
                &[("demo", "/Users/me/demo"), ("demo", "/opt/code/demo")],
                Some("/Users/me"),
            ),
            [Some("~".into()), Some("/opt/code".into())]
        );
        // Two projects in one folder show their own paths.
        assert_eq!(
            hints(
                &[("demo", "/Users/me/code/a"), ("demo", "/Users/me/code/b")],
                Some("/Users/me"),
            ),
            [Some("~/code/a".into()), Some("~/code/b".into())]
        );
    }

    #[test]
    fn long_paths_lose_folders_from_the_left_only_while_still_apart() {
        assert_eq!(
            hints(
                &[
                    ("demo", "/Users/me/work/clients/acme/src/a/demo"),
                    ("demo", "/Users/me/work/clients/acme/src/b/demo"),
                ],
                Some("/Users/me"),
            ),
            [Some("…/acme/src/a".into()), Some("…/acme/src/b".into())]
        );
        // They differ further left: only what tells them apart stays.
        assert_eq!(
            hints(
                &[
                    ("demo", "/Users/me/x/one/two/three/demo"),
                    ("demo", "/Users/me/y/one/two/three/demo"),
                ],
                Some("/Users/me"),
            ),
            [
                Some("…/x/one/two/three".into()),
                Some("…/y/one/two/three".into())
            ]
        );
    }
}
