//! Add project: pick a folder (or drop one on the dialog), see what it
//! holds, choose how agents work there and add it with its `#general`.

use std::path::{Path, PathBuf};

use crew_core::ids::{ChannelId, ProjectId};
use crew_core::project_kind::{NEEDS_REPO, ProjectKind, WorkMode, is_live_keys, looks_secret};
use crew_proto::{FolderInfo, Request, SetupFile, SetupFileSize};
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::checkbox::Checkbox;
use gpui_kit::component::input::{Input, InputState};
use gpui_kit::component::radio::Radio;
use gpui_kit::component::{Icon, IconName, Sizable as _, WindowExt as _};
use gpui_kit::prelude::FluentBuilder as _;
use gpui_kit::*;

use super::super::first_launch::AddedFolder;
use super::super::settings::projects::FIRST_CHANNEL;
use super::super::title_bar::home_relative;
use super::super::workspace::{Place, Workspace};
use super::{DialogKind, field_label, heading, hint};
use crate::theme::color;
use crate::theme::scale::{self, code_family, ui_px, weight};

/// The dialog's width, from the board.
pub(super) const WIDTH: f32 = 560.;
/// Repos named in "Folder with {n} git repos inside", before "and {n} more".
const NAMED_REPOS: usize = 3;
/// Files counted in a setup folder you add, at most.
const MAX_COUNTED: u32 = 10_000;

pub(super) const NO_NAME: &str = "Give the project a name.";
pub(crate) const NOT_A_FOLDER: &str = "That isn't a folder. Choose another one.";
const PICK_A_FOLDER: &str =
    "Pick a folder. A git repo, a folder of repos and a plain folder all work.";
const READING: &str = "Reading folder…";
const WARNING: &str = "Agents edit your files directly. There's no undo outside git.";

/// What the Add project dialog holds.
pub(crate) struct AddProject {
    /// The folder picked, once crewd said what it holds.
    pub(super) picked: Option<Picked>,
    /// The folder crewd is being asked about, shown as "Reading folder…".
    pub(super) reading: Option<PathBuf>,
    /// Counts the folders asked about, so only the last answer lands.
    pub(super) seq: u64,
    /// Asking crewd about a folder, or looking at a setup file you added.
    task: Option<Task<()>>,
}

pub(super) struct Picked {
    /// The folder to add: the repo's top folder when you picked a folder
    /// inside a repo.
    pub(super) path: PathBuf,
    /// You picked a folder inside the repo at `path`.
    pub(super) inside_repo: bool,
    pub(super) info: FolderInfo,
    pub(super) name: Entity<InputState>,
    pub(super) mode: WorkMode,
    /// The setup files on offer, and whether each is copied.
    pub(super) files: Vec<(SetupFile, bool)>,
    pub(super) setup: Entity<InputState>,
}

impl Picked {
    fn valid(&self) -> bool {
        self.info.exists && self.info.already_added.is_none()
    }
}

/// How a setup file's size reads: "412 B", "1 KB", "4 files".
pub fn size_label(size: SetupFileSize) -> String {
    match size {
        SetupFileSize::Missing => "missing".into(),
        SetupFileSize::File { bytes } if bytes < 1024 => format!("{bytes} B"),
        SetupFileSize::File { bytes } if bytes < 1024 * 1024 => {
            format!("{} KB", (bytes as f64 / 1024.).round() as u64)
        }
        SetupFileSize::File { bytes } => {
            format!("{} MB", (bytes as f64 / (1024. * 1024.)).round() as u64)
        }
        SetupFileSize::Folder { files: 1 } => "1 file".into(),
        SetupFileSize::Folder { files } => format!("{files} files"),
    }
}

/// What a setup file's row says on its right: its size, or why it starts
/// left out.
pub fn file_note(file: &SetupFile, checked: bool) -> String {
    match (file.looks_secret && !checked, is_live_keys(&file.path)) {
        (true, true) => "live keys, left out".into(),
        (true, false) => "looks secret, left out".into(),
        (false, _) => size_label(file.size),
    }
}

/// Where `origin` points, as the board writes it: `github.com/owner/api`.
pub fn short_remote(url: &str) -> String {
    let url = url.trim();
    let (rest, scheme) = match url.split_once("://") {
        Some((_, rest)) => (rest, true),
        None => (url, false),
    };
    let rest = match rest.split_once('@') {
        Some((user, host)) if !user.contains('/') => host,
        _ => rest,
    };
    let rest = if scheme {
        rest.to_string()
    } else {
        rest.replacen(':', "/", 1)
    };
    rest.trim_end_matches('/')
        .trim_end_matches(".git")
        .to_string()
}

/// The repos a folder holds, the first few by name.
pub fn repos_line(repos: &[String]) -> String {
    let mut named: Vec<&str> = repos.iter().map(String::as_str).collect();
    named.sort_unstable();
    let shown = named[..named.len().min(NAMED_REPOS)].join(", ");
    let more = repos.len().saturating_sub(NAMED_REPOS);
    let what = format!("Folder with {} git repos inside: {shown}", repos.len());
    if more > 0 {
        format!("{what} and {more} more")
    } else {
        what
    }
}

/// The chip of a recent folder.
pub fn kind_chip(kind: &ProjectKind) -> String {
    match kind {
        ProjectKind::Repo => "git".into(),
        ProjectKind::MultiRepo { repos } => format!("{} repos", repos.len()),
        ProjectKind::Plain => "folder".into(),
    }
}

/// A work mode's title and help, for a folder of `kind`.
pub fn mode_copy(kind: &ProjectKind, mode: WorkMode) -> (&'static str, &'static str) {
    match (kind, mode) {
        (ProjectKind::Repo, WorkMode::OwnCopy) => (
            "Own worktree per thread (recommended)",
            "Each thread gets its own branch and folder, so threads never collide.",
        ),
        (ProjectKind::MultiRepo { .. }, WorkMode::OwnCopy) => (
            "Own copy per thread (recommended)",
            "Crew makes a thread folder with a worktree of each repo, so threads don't collide. Plain files outside the repos are not copied.",
        ),
        (ProjectKind::Plain, WorkMode::OwnCopy) => ("Own copy per thread", NEEDS_REPO),
        (ProjectKind::Repo, WorkMode::InPlace) => (
            "In the folder itself",
            "Agents edit your checkout. One writing agent at a time.",
        ),
        (ProjectKind::MultiRepo { .. }, WorkMode::InPlace) => (
            "In the folder itself",
            "Agents edit files in place and all threads share the folder. One writing agent at a time; others wait.",
        ),
        (ProjectKind::Plain, WorkMode::InPlace) => (
            "In the folder itself",
            "Agents edit files in place. One writing agent at a time; others wait.",
        ),
    }
}

/// The folder's own name.
fn folder_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string())
}

/// A setup file you picked, relative to the project folder, with its
/// size. Reads the disk, so off the UI thread.
pub(crate) fn setup_file_in(root: &Path, picked: &Path) -> Option<SetupFile> {
    let root = root.canonicalize().ok()?;
    let picked = picked.canonicalize().ok()?;
    let rel = picked.strip_prefix(&root).ok()?;
    if rel.as_os_str().is_empty() {
        return None;
    }
    let meta = std::fs::metadata(&picked).ok()?;
    let mut path = rel.to_string_lossy().into_owned();
    let size = if meta.is_dir() {
        path.push('/');
        SetupFileSize::Folder {
            files: count_files(&picked),
        }
    } else {
        SetupFileSize::File { bytes: meta.len() }
    };
    Some(SetupFile {
        looks_secret: looks_secret(&path),
        path,
        size,
        ignored: false,
    })
}

/// Files under `dir`, counted up to [`MAX_COUNTED`].
fn count_files(dir: &Path) -> u32 {
    let mut count = 0;
    let mut todo = vec![dir.to_path_buf()];
    while let Some(dir) = todo.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(t) if t.is_dir() => todo.push(entry.path()),
                Ok(_) => count += 1,
                Err(_) => {}
            }
            if count >= MAX_COUNTED {
                return count;
            }
        }
    }
    count
}

impl Workspace {
    /// Opens Add project at its first step: pick a folder.
    pub(crate) fn open_add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.show_dialog(
            DialogKind::AddProject(Box::new(AddProject {
                picked: None,
                reading: None,
                seq: 0,
                task: None,
            })),
            window,
            cx,
        );
    }

    /// Opens Add project and its folder picker at once, as the first
    /// launch does.
    pub(crate) fn open_add_project_and_pick(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.open_add_project(window, cx);
        self.add_project_choose(window, cx);
    }

    fn add_project(&mut self) -> Option<&mut AddProject> {
        match self.dialog.as_mut().map(|d| &mut d.kind) {
            Some(DialogKind::AddProject(add)) => Some(add),
            _ => None,
        }
    }

    /// Whether Add project can add the folder shown.
    pub(in crate::ui) fn add_project_ready(&self) -> bool {
        match self.dialog.as_ref().map(|d| &d.kind) {
            Some(DialogKind::AddProject(add)) => add.picked.as_ref().is_some_and(Picked::valid),
            _ => false,
        }
    }

    /// How many folders Add project asked crewd about.
    #[cfg(test)]
    pub(crate) fn add_project_seq(&self) -> u64 {
        match self.dialog.as_ref().map(|d| &d.kind) {
            Some(DialogKind::AddProject(add)) => add.seq,
            _ => 0,
        }
    }

    /// The name typed in Add project, once a folder is picked.
    #[cfg(test)]
    pub(crate) fn add_project_name(&self, cx: &App) -> Option<String> {
        match self.dialog.as_ref().map(|d| &d.kind) {
            Some(DialogKind::AddProject(add)) => add
                .picked
                .as_ref()
                .map(|p| p.name.read(cx).value().to_string()),
            _ => None,
        }
    }

    /// Opens the folder picker, then asks crewd about the folder.
    fn add_project_choose(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // One picker at a time.
        if self.folder_task.is_some() {
            return;
        }
        let picked = (self.pick_folder)(cx);
        self.folder_task = Some(cx.spawn_in(window, async move |this, cx| {
            let picked = picked.await;
            this.update_in(cx, |this, window, cx| {
                this.folder_task = None;
                if let Some(dir) = picked {
                    this.add_project_inspect(dir, window, cx);
                }
                cx.notify();
            })
            .ok();
        }));
    }

    /// While Add project is open, a folder dropped anywhere on the window,
    /// the whole dialog included, is picked. The dialog's layer sits above
    /// the window, so the drop is followed from the window's root: drag
    /// moves note the paths, and the mouse up that ends the drag picks
    /// them.
    pub(in crate::ui) fn add_project_drop_target(
        &self,
        root: Stateful<Div>,
        cx: &mut Context<Self>,
    ) -> Stateful<Div> {
        if !matches!(
            self.dialog.as_ref().map(|d| &d.kind),
            Some(DialogKind::AddProject(_))
        ) {
            return root;
        }
        root.on_drag_move(
            cx.listener(|this, event: &DragMoveEvent<ExternalPaths>, _, cx| {
                this.dragged_paths = Some(event.drag(cx).clone());
            }),
        )
        .on_mouse_up(
            MouseButton::Left,
            cx.listener(|this, _, window, cx| this.add_project_drag_ended(window, cx)),
        )
        .on_mouse_up_out(
            MouseButton::Left,
            cx.listener(|this, _, window, cx| this.add_project_drag_ended(window, cx)),
        )
    }

    fn add_project_drag_ended(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let dragged = self.dragged_paths.take();
        if let Some(paths) = dragged.filter(|_| cx.has_active_drag()) {
            self.add_project_dropped(&paths, window, cx);
        }
    }

    /// A folder dropped on the dialog.
    fn add_project_dropped(
        &mut self,
        paths: &ExternalPaths,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = paths.paths().first() {
            self.add_project_inspect(path.clone(), window, cx);
        }
    }

    /// Asks crewd what `path` holds and shows it.
    pub(in crate::ui) fn add_project_inspect(
        &mut self,
        path: PathBuf,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(add) = self.add_project() else {
            return;
        };
        add.seq += 1;
        let seq = add.seq;
        // What showed is gone: Add stays off until this answer lands.
        add.picked = None;
        add.reading = Some(path.clone());
        if let Some(open) = &mut self.dialog {
            open.error = None;
        }
        cx.notify();
        let answer = self.client.request(Request::InspectFolder {
            path: path.to_string_lossy().into_owned(),
        });
        let task = cx.spawn_in(window, async move |this, cx| {
            let info = answer
                .await
                .and_then(|v| serde_json::from_value::<FolderInfo>(v).map_err(|e| e.to_string()));
            this.update_in(cx, |this, window, cx| {
                this.add_project_answer(seq, path, info, window, cx)
            })
            .ok();
        });
        if let Some(add) = self.add_project() {
            add.task = Some(task);
        }
    }

    /// crewd's answer about the folder asked `seq`-th. A later ask makes
    /// it stale, and it is dropped.
    pub(in crate::ui) fn add_project_answer(
        &mut self,
        seq: u64,
        path: PathBuf,
        info: Result<FolderInfo, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(add) = self.add_project().filter(|a| a.seq == seq) else {
            return;
        };
        add.reading = None;
        self.add_project_inspected(path, info, window, cx);
    }

    fn add_project_inspected(
        &mut self,
        path: PathBuf,
        info: Result<FolderInfo, String>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let info = match info {
            Ok(info) if info.exists => info,
            Ok(_) => return self.add_project_error(NOT_A_FOLDER.into(), cx),
            Err(e) => return self.add_project_error(e, cx),
        };
        let root = if info.root.is_empty() {
            path.clone()
        } else {
            PathBuf::from(&info.root)
        };
        let inside_repo = root != path;
        let path = root;
        if self.recents.picked_folder(&path, info.kind.clone()) {
            self.save_recents(cx);
        }
        let suggestion = if info.name_suggestion.is_empty() {
            folder_name(&path)
        } else {
            info.name_suggestion.clone()
        };
        let name = cx.new(|cx| InputState::new(window, cx).default_value(suggestion));
        let setup = cx.new(|cx| InputState::new(window, cx).placeholder("pnpm install"));
        let files = info
            .setup_files
            .iter()
            .map(|f| (f.clone(), !f.looks_secret))
            .collect();
        let valid = info.already_added.is_none();
        let mode = info.kind.default_mode();
        if let Some(open) = &mut self.dialog {
            open.error = None;
        }
        if let Some(add) = self.add_project() {
            add.picked = Some(Picked {
                path,
                inside_repo,
                info,
                name: name.clone(),
                mode,
                files,
                setup,
            });
        }
        if valid {
            name.update(cx, |input, cx| input.focus(window, cx));
        }
        cx.notify();
    }

    fn add_project_error(&mut self, error: String, cx: &mut Context<Self>) {
        if let Some(open) = &mut self.dialog {
            open.error = Some(error);
        }
        cx.notify();
    }

    fn add_project_mode(&mut self, mode: WorkMode, cx: &mut Context<Self>) {
        if let Some(picked) = self.add_project().and_then(|a| a.picked.as_mut())
            && picked.info.kind.allows(mode).is_ok()
        {
            picked.mode = mode;
            cx.notify();
        }
    }

    fn add_project_toggle_file(&mut self, ix: usize, cx: &mut Context<Self>) {
        if let Some((_, checked)) = self
            .add_project()
            .and_then(|a| a.picked.as_mut())
            .and_then(|p| p.files.get_mut(ix))
        {
            *checked = !*checked;
            cx.notify();
        }
    }

    /// Picks a file or folder inside the project folder to copy too.
    fn add_project_add_file(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.folder_task.is_some() {
            return;
        }
        let Some(root) = self
            .add_project()
            .and_then(|a| a.picked.as_ref())
            .map(|p| p.path.clone())
        else {
            return;
        };
        let picked = (self.pick_file)(cx);
        let background = cx.background_executor().clone();
        self.folder_task = Some(cx.spawn_in(window, async move |this, cx| {
            let found = match picked.await {
                Some(file) => {
                    let root = root.clone();
                    Some(
                        background
                            .spawn(async move { setup_file_in(&root, &file) })
                            .await,
                    )
                }
                None => None,
            };
            this.update(cx, |this, cx| {
                this.folder_task = None;
                match found {
                    Some(Some(file)) => this.add_project_file_found(&root, file, cx),
                    Some(None) => this.add_project_error(
                        format!("Pick a file inside {}.", folder_name(&root)),
                        cx,
                    ),
                    None => {}
                }
                cx.notify();
            })
            .ok();
        }));
    }

    fn add_project_file_found(&mut self, root: &Path, file: SetupFile, cx: &mut Context<Self>) {
        if let Some(open) = &mut self.dialog {
            open.error = None;
        }
        let Some(picked) = self
            .add_project()
            .and_then(|a| a.picked.as_mut())
            .filter(|p| p.path == root)
        else {
            return;
        };
        match picked.files.iter_mut().find(|(f, _)| f.path == file.path) {
            Some((_, checked)) => *checked = true,
            None => picked.files.push((file, true)),
        }
        cx.notify();
    }

    /// "Open it": the project that already has the folder.
    fn add_project_open_existing(
        &mut self,
        project: ProjectId,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.dialog = None;
        window.close_dialog(cx);
        if self.first_launch.is_some()
            && let Some(stored) = self.state().project(project).cloned()
        {
            let added = AddedFolder {
                name: stored.name,
                branch: (!stored.main_branch.is_empty()).then_some(stored.main_branch),
                root: stored.root,
            };
            self.first_launch_added(project, None, added, window, cx);
            return;
        }
        self.open_place(Place::Project(project), window, cx);
    }

    /// Adds the folder picked, then its `#general`, and opens it. In the
    /// first launch, step 1 is done instead.
    pub(super) fn confirm_add_project(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(open) = &self.dialog else {
            return;
        };
        let DialogKind::AddProject(add) = &open.kind else {
            return;
        };
        let Some(picked) = add.picked.as_ref().filter(|p| p.valid()) else {
            return;
        };
        let name = picked.name.read(cx).value().trim().to_string();
        if name.is_empty() {
            let input = picked.name.clone();
            self.add_project_error(NO_NAME.into(), cx);
            input.update(cx, |input, cx| input.focus(window, cx));
            return;
        }
        let own_copy = picked.mode == WorkMode::OwnCopy;
        let copy_files: Vec<String> = if own_copy {
            picked
                .files
                .iter()
                .filter(|(_, checked)| *checked)
                .map(|(f, _)| f.path.trim_end_matches('/').to_string())
                .collect()
        } else {
            Vec::new()
        };
        let setup = picked.setup.read(cx).value().trim().to_string();
        let request = Request::CreateProject {
            name: name.clone(),
            root: picked.path.to_string_lossy().into_owned(),
            main_branch: String::new(),
            work_mode: Some(picked.mode),
            copy_files,
            setup_command: (own_copy && !setup.is_empty()).then_some(setup),
        };
        let added = AddedFolder {
            name,
            root: picked.path.clone(),
            branch: picked.info.branch.clone(),
        };
        let kind = picked.info.kind.clone();
        let client = self.client.clone();
        let task = cx.spawn_in(window, async move |this, cx| {
            let project = client
                .request(request)
                .await
                .and_then(|v| serde_json::from_value::<ProjectId>(v).map_err(|e| e.to_string()));
            let channel = match &project {
                Ok(project) => client
                    .request(Request::CreateChannel {
                        project: *project,
                        name: FIRST_CHANNEL.into(),
                    })
                    .await
                    .and_then(|v| serde_json::from_value::<ChannelId>(v).map_err(|e| e.to_string()))
                    .ok(),
                Err(_) => None,
            };
            this.update_in(cx, |this, window, cx| {
                match project {
                    Ok(project) => {
                        this.dialog = None;
                        window.close_dialog(cx);
                        if this.recents.picked_folder(&added.root, kind) {
                            this.save_recents(cx);
                        }
                        this.project_added(project, channel, added, window, cx);
                    }
                    Err(error) => {
                        if let Some(open) = &mut this.dialog {
                            open.busy = false;
                            open.error = Some(error);
                            open.task = None;
                        }
                    }
                }
                cx.notify();
            })
            .ok();
        });
        if let Some(open) = &mut self.dialog {
            open.busy = true;
            open.error = None;
            open.task = Some(task);
        }
        cx.notify();
    }

    /// Lands in the new project's `#general`, or its first channel when
    /// it came back with channels of its own.
    fn project_added(
        &mut self,
        project: ProjectId,
        channel: Option<ChannelId>,
        added: AddedFolder,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.first_launch.is_some() {
            self.first_launch_added(project, channel, added, window, cx);
            return;
        }
        match channel {
            Some(channel) => self.open_place(Place::Channel(channel), window, cx),
            None => self.open_place(Place::Project(project), window, cx),
        }
    }

    /// The dialog's body: the drop zone and recent folders, or what the
    /// folder picked holds and how agents work there.
    pub(super) fn add_project_body(&self, add: &AddProject, this: WeakEntity<Self>) -> AnyElement {
        let body = div().id("add-project").flex().flex_col().gap(ui_px(16.));
        let body = match (&add.picked, &add.reading) {
            (None, Some(path)) => body
                .child(heading(
                    "add-project-heading",
                    "Add project".into(),
                    None,
                    "Add project".into(),
                ))
                .child(folder_with(path, false, reading_line(), this)),
            (None, None) => body
                .child(heading(
                    "add-project-heading",
                    "Add project".into(),
                    Some(
                        div()
                            .id("add-project-about-kinds")
                            .aria_label(PICK_A_FOLDER)
                            .child(PICK_A_FOLDER)
                            .test_support()
                            .into_any_element(),
                    ),
                    "Add project".into(),
                ))
                .child(drop_zone(this.clone()))
                .children(self.recent_folders(this)),
            (Some(picked), _) => body
                .child(heading(
                    "add-project-heading",
                    "Add project".into(),
                    None,
                    "Add project".into(),
                ))
                .children(self.picked_fields(picked, this)),
        };
        body.test_support().into_any_element()
    }

    fn recent_folders(&self, this: WeakEntity<Self>) -> Option<AnyElement> {
        let folders = &self.recents.folders;
        if folders.is_empty() {
            return None;
        }
        let rows = folders.iter().enumerate().map(|(ix, folder)| {
            let name = folder_name(&folder.path);
            let chip_text = kind_chip(&folder.kind);
            let path = folder.path.clone();
            let this = this.clone();
            div()
                .id(("add-project-recent", ix))
                .flex()
                .items_center()
                .gap(ui_px(10.))
                .h(scale::row(40.))
                .px(ui_px(10.))
                .rounded(ui_px(6.))
                .cursor_pointer()
                .hover(|row| row.bg(color("surface-300")))
                .aria_label(format!("{name} {chip_text}"))
                .child(folder_icon(14.))
                .child(div().font_weight(weight(FontWeight::MEDIUM)).child(name))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(code_family())
                        .text_size(scale::mono(11.))
                        .text_color(color("text-tertiary"))
                        .child(home_relative(&folder.path)),
                )
                .child(crate::ui::common::chip(chip_text))
                .on_click(move |_, window, cx| {
                    let path = path.clone();
                    this.update(cx, |ws, cx| ws.add_project_inspect(path, window, cx))
                        .ok();
                })
                .test_support()
                .into_any_element()
        });
        Some(
            div()
                .flex()
                .flex_col()
                .gap(ui_px(6.))
                .child(label("Recent folders"))
                .child(div().flex().flex_col().gap(ui_px(2.)).children(rows))
                .into_any_element(),
        )
    }

    fn picked_fields(&self, picked: &Picked, this: WeakEntity<Self>) -> Vec<AnyElement> {
        let mut fields = vec![self.folder_field(picked, this.clone())];
        if !picked.valid() {
            return fields;
        }
        fields.push(
            div()
                .flex()
                .flex_col()
                .gap(ui_px(6.))
                .child(label("Name"))
                .child(
                    div()
                        .id("add-project-name")
                        .child(Input::new(&picked.name).aria_label("Name"))
                        .test_support(),
                )
                .into_any_element(),
        );
        fields.push(mode_field(picked, this.clone()));
        match picked.mode {
            WorkMode::InPlace => fields.push(warning()),
            WorkMode::OwnCopy => {
                fields.push(setup_files_field(picked, this));
                fields.push(setup_command_field(picked));
            }
        }
        fields.push(
            hint()
                .id("add-project-general")
                .flex()
                .aria_label("Crew creates #general for you.")
                .child("Crew creates ")
                .child(
                    div()
                        .text_color(color("text-primary"))
                        .child(format!("#{FIRST_CHANNEL}")),
                )
                .child(" for you.")
                .test_support()
                .into_any_element(),
        );
        fields
    }

    /// The folder's path with Change, and what it holds or who has it.
    fn folder_field(&self, picked: &Picked, this: WeakEntity<Self>) -> AnyElement {
        let taken = picked.info.already_added;
        let below = match taken {
            Some(project) => {
                let name = self
                    .state()
                    .project(project)
                    .map(|p| p.name.clone())
                    .unwrap_or_else(|| picked.info.name_suggestion.clone());
                taken_line(project, name, this.clone())
            }
            None if picked.inside_repo => inside_repo_line(&picked.path),
            None => about_line(&picked.info),
        };
        folder_with(&picked.path, taken.is_some(), below, this)
    }
}

/// The Folder field: label, the path with Change, and `below`.
fn folder_with(
    path: &Path,
    taken: bool,
    below: AnyElement,
    this: WeakEntity<Workspace>,
) -> AnyElement {
    let path_row = div()
        .id("add-project-folder")
        .flex()
        .items_center()
        .gap(ui_px(8.))
        .h(scale::row(32.))
        .pl(ui_px(10.))
        .pr(ui_px(4.))
        .border_1()
        .border_color(color(if taken { "danger" } else { "border-200" }))
        .rounded(ui_px(6.))
        .bg(color("bg-100"))
        .aria_label(path.to_string_lossy().into_owned())
        .child(
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .font_family(code_family())
                .text_size(scale::mono(12.))
                .child(home_relative(path)),
        )
        .child(
            Button::new("add-project-change")
                .ghost()
                .small()
                .label("Change")
                .on_click(move |_, window, cx| {
                    this.update(cx, |ws, cx| ws.add_project_choose(window, cx))
                        .ok();
                }),
        )
        .test_support();
    // The board's focus ring, in red.
    let path_row = if taken {
        div()
            .rounded(ui_px(8.))
            .p(ui_px(3.))
            .m(ui_px(-3.))
            .bg(color("danger").opacity(0.2))
            .child(path_row)
            .into_any_element()
    } else {
        path_row.into_any_element()
    };
    div()
        .flex()
        .flex_col()
        .gap(ui_px(6.))
        .child(label("Folder"))
        .child(path_row)
        .child(below)
        .into_any_element()
}

/// Under the folder while crewd reads it.
fn reading_line() -> AnyElement {
    hint()
        .id("add-project-reading")
        .aria_label(READING)
        .child(READING)
        .test_support()
        .into_any_element()
}

/// Under the folder when you picked a folder inside a repo.
fn inside_repo_line(root: &Path) -> AnyElement {
    let text = inside_repo_note(&folder_name(root));
    hint()
        .id("add-project-about")
        .flex()
        .items_center()
        .gap(ui_px(6.))
        .aria_label(text.clone())
        .child(
            Icon::new(IconName::Check)
                .size(ui_px(12.))
                .text_color(color("success")),
        )
        .child(text)
        .test_support()
        .into_any_element()
}

/// The note for a folder inside the repo `repo`.
pub fn inside_repo_note(repo: &str) -> String {
    format!("Inside the {repo} repo, so Crew adds the whole repo.")
}

/// A field's label, findable by its text.
fn label(text: &'static str) -> AnyElement {
    field_label(text)
        .id(SharedString::from(format!("add-project-label-{text}")))
        .aria_label(text)
        .test_support()
        .into_any_element()
}

fn folder_icon(size: f32) -> Icon {
    Icon::new(IconName::Folder)
        .size(ui_px(size))
        .text_color(color("text-secondary"))
}

fn mono_span(text: impl Into<SharedString>) -> Div {
    div()
        .font_family(code_family())
        .text_color(color("text-primary"))
        .child(text.into())
}

/// Step 1: "Choose a folder", "or drag one here", Choose….
fn drop_zone(this: WeakEntity<Workspace>) -> AnyElement {
    div()
        .id("add-project-drop")
        .flex()
        .flex_col()
        .items_center()
        .justify_center()
        .gap(ui_px(8.))
        .h(ui_px(180.))
        .border_1()
        .border_dashed()
        .border_color(color("border-200"))
        .rounded(ui_px(8.))
        .bg(color("bg-100"))
        .drag_over::<ExternalPaths>(|style, _, _, _| {
            style
                .border_color(color("accent-blue"))
                .bg(color("info-subtle"))
        })
        .child(folder_icon(24.))
        .child(
            div()
                .id("add-project-choose-title")
                .text_size(scale::text(14.))
                .font_weight(weight(FontWeight::SEMIBOLD))
                .aria_label("Choose a folder")
                .child("Choose a folder")
                .test_support(),
        )
        .child(
            hint()
                .id("add-project-drag-hint")
                .aria_label("or drag one here")
                .child("or drag one here")
                .test_support(),
        )
        .child(
            div().mt(ui_px(4.)).child(
                Button::new("add-project-choose")
                    .outline()
                    .label("Choose…")
                    .on_click(move |_, window, cx| {
                        this.update(cx, |ws, cx| ws.add_project_choose(window, cx))
                            .ok();
                    }),
            ),
        )
        .test_support()
        .into_any_element()
}

/// Under the folder: what it holds, with a check.
fn about_line(info: &FolderInfo) -> AnyElement {
    let check = || {
        Icon::new(IconName::Check)
            .size(ui_px(12.))
            .text_color(color("success"))
    };
    let line = hint()
        .id("add-project-about")
        .flex()
        .flex_wrap()
        .items_center();
    let line = match &info.kind {
        ProjectKind::Repo => {
            let mut label = "Git repo".to_string();
            let mut line = line.gap(ui_px(6.)).child(check());
            let mut words = div().flex().flex_wrap().gap(ui_px(4.)).child("Git repo");
            if let Some(branch) = &info.branch {
                label.push_str(&format!(" on {branch}"));
                words = words.child("on").child(mono_span(branch.clone()));
            }
            if let Some(remote) = &info.remote {
                let remote = short_remote(remote);
                label.push_str(&format!(" · origin {remote}"));
                words = words.child("· origin").child(mono_span(remote));
            }
            line = line.child(words);
            line.aria_label(label)
        }
        ProjectKind::MultiRepo { repos } => {
            let mut named: Vec<&String> = repos.iter().collect();
            named.sort_unstable();
            let more = repos.len().saturating_sub(NAMED_REPOS);
            let mut words = div()
                .flex()
                .flex_wrap()
                .gap(ui_px(4.))
                .child(format!("Folder with {} git repos inside:", repos.len()));
            for (ix, repo) in named.iter().take(NAMED_REPOS).enumerate() {
                let last = ix + 1 == named.len().min(NAMED_REPOS);
                words = words.child(
                    div()
                        .flex()
                        .child(mono_span((*repo).clone()))
                        .when(!last, |d| d.child(",")),
                );
            }
            if more > 0 {
                words = words.child(format!("and {more} more"));
            }
            line.gap(ui_px(6.))
                .child(check())
                .child(words)
                .aria_label(repos_line(repos))
        }
        ProjectKind::Plain => line
            .aria_label("Plain folder, no git")
            .child("Plain folder, no git"),
    };
    line.test_support().into_any_element()
}

/// Under the folder when a project has it already.
fn taken_line(project: ProjectId, name: String, this: WeakEntity<Workspace>) -> AnyElement {
    div()
        .id("add-project-taken")
        .flex()
        .flex_wrap()
        .items_center()
        .gap(ui_px(6.))
        .text_size(scale::text(12.))
        .line_height(scale::line(18.))
        .text_color(color("danger"))
        .child(
            Icon::new(IconName::CircleAlert)
                .size(ui_px(12.))
                .text_color(color("danger")),
        )
        .child(
            div()
                .id("add-project-already")
                .flex()
                .aria_label(format!("Already added as {name}."))
                .child("Already added as ")
                .child(div().font_family(code_family()).child(name))
                .child(".")
                .test_support(),
        )
        .child(
            div()
                .id("add-project-open")
                .text_color(color("accent-blue"))
                .cursor_pointer()
                .aria_label("Open it")
                .child("Open it")
                .on_click(move |_, window, cx| {
                    this.update(cx, |ws, cx| {
                        ws.add_project_open_existing(project, window, cx)
                    })
                    .ok();
                })
                .test_support(),
        )
        .test_support()
        .into_any_element()
}

/// "How agents work here": the modes the folder allows, the default
/// first.
fn mode_field(picked: &Picked, this: WeakEntity<Workspace>) -> AnyElement {
    let kind = &picked.info.kind;
    let order = match kind {
        ProjectKind::Plain => [WorkMode::InPlace, WorkMode::OwnCopy],
        _ => [WorkMode::OwnCopy, WorkMode::InPlace],
    };
    let cards = order.map(|mode| {
        let (title, help) = mode_copy(kind, mode);
        let selected = picked.mode == mode;
        let enabled = kind.allows(mode).is_ok();
        let key = match mode {
            WorkMode::OwnCopy => "own-copy",
            WorkMode::InPlace => "in-place",
        };
        let label = if selected {
            format!("{title} (selected)")
        } else if !enabled {
            format!("{title} (disabled)")
        } else {
            title.to_string()
        };
        let pick = {
            let this = this.clone();
            move |cx: &mut App| {
                this.update(cx, |ws, cx| ws.add_project_mode(mode, cx)).ok();
            }
        };
        let radio_pick = pick.clone();
        div()
            .id(SharedString::from(format!("add-project-mode-{key}")))
            .flex()
            .items_start()
            .gap(ui_px(10.))
            .px(ui_px(12.))
            .py(scale::row(10.))
            .border_1()
            .border_color(color(if selected { "border-200" } else { "border-100" }))
            .rounded(ui_px(6.))
            .when(selected, |card| card.bg(color("surface-200")))
            .when(!enabled, |card| card.opacity(0.5))
            .when(enabled, |card| card.cursor_pointer())
            .role(Role::RadioButton)
            .aria_label(label)
            .child(
                div().mt(ui_px(3.)).child(
                    Radio::new(SharedString::from(format!("add-project-radio-{key}")))
                        .checked(selected)
                        .disabled(!enabled)
                        .on_click(move |_, _, cx| radio_pick(cx)),
                ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .min_w_0()
                    .child(
                        div()
                            .line_height(scale::line(20.))
                            .font_weight(weight(FontWeight::MEDIUM))
                            .child(title),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("add-project-mode-{key}-help")))
                            .text_size(scale::text(12.))
                            .line_height(scale::line(18.))
                            .text_color(color("text-tertiary"))
                            .aria_label(help)
                            .child(help)
                            .test_support(),
                    ),
            )
            .when(enabled, |card| card.on_click(move |_, _, cx| pick(cx)))
            .test_support()
            .into_any_element()
    });
    div()
        .flex()
        .flex_col()
        .gap(ui_px(6.))
        .child(label("How agents work here"))
        .child(
            div()
                .id("add-project-modes")
                .flex()
                .flex_col()
                .gap(ui_px(6.))
                .role(Role::RadioGroup)
                .aria_label("How agents work here")
                .children(cards)
                .test_support(),
        )
        .into_any_element()
}

/// The in-place callout.
fn warning() -> AnyElement {
    div()
        .id("add-project-warning")
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
        .aria_label(WARNING)
        .child(
            div().mt(ui_px(2.)).flex_none().child(
                Icon::new(IconName::TriangleAlert)
                    .size(ui_px(14.))
                    .text_color(color("warning")),
            ),
        )
        .child(WARNING)
        .test_support()
        .into_any_element()
}

/// The setup files checklist and Add another file.
fn setup_files_field(picked: &Picked, this: WeakEntity<Workspace>) -> AnyElement {
    let count = picked.files.len();
    let rows = picked
        .files
        .iter()
        .enumerate()
        .map(|(ix, (file, checked))| {
            let checked = *checked;
            let note = file_note(file, checked);
            let left_out = file.looks_secret && !checked;
            let label = if checked {
                format!("{} {note} (checked)", file.path)
            } else {
                format!("{} {note}", file.path)
            };
            let this = this.clone();
            div()
                .id(("add-project-file", ix))
                .flex()
                .items_center()
                .gap(ui_px(10.))
                .h(scale::row(32.))
                .px(ui_px(10.))
                .when(ix + 1 < count, |row| {
                    row.border_b_1().border_color(color("border-100"))
                })
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .cursor_pointer()
                .aria_label(label)
                .child(Checkbox::new(("add-project-check", ix)).checked(checked))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .font_family(code_family())
                        .text_color(color("text-primary"))
                        .child(file.path.clone()),
                )
                .child(
                    div()
                        .when(left_out, |d| d.text_color(color("warning")))
                        .child(note),
                )
                .on_click(move |_, _, cx| {
                    this.update(cx, |ws, cx| ws.add_project_toggle_file(ix, cx))
                        .ok();
                })
                .test_support()
                .into_any_element()
        });
    let found = picked.files.iter().any(|(f, _)| f.ignored);
    div()
        .flex()
        .flex_col()
        .gap(ui_px(6.))
        .child(label("Setup files to copy into each worktree"))
        .when(count > 0, |field| {
            field.child(
                div()
                    .border_1()
                    .border_color(color("border-100"))
                    .rounded(ui_px(6.))
                    .overflow_hidden()
                    .children(rows),
            )
        })
        .child(
            hint()
                .flex()
                .flex_wrap()
                .gap(ui_px(4.))
                .when(found, |line| {
                    line.child(
                        div()
                            .id("add-project-found")
                            .aria_label("Found in the folder and ignored by git.")
                            .child("Found in the folder and ignored by git.")
                            .test_support(),
                    )
                })
                .child(
                    div()
                        .id("add-project-add-file")
                        .text_color(color("accent-blue"))
                        .cursor_pointer()
                        .aria_label("Add another file")
                        .child("Add another file")
                        .on_click(move |_, window, cx| {
                            this.update(cx, |ws, cx| ws.add_project_add_file(window, cx))
                                .ok();
                        })
                        .test_support(),
                ),
        )
        .into_any_element()
}

fn setup_command_field(picked: &Picked) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(ui_px(6.))
        .child(
            div()
                .id("add-project-setup-label")
                .flex()
                .gap(ui_px(4.))
                .aria_label("Setup command · optional")
                .child(field_label("Setup command"))
                .child(
                    div()
                        .text_size(scale::text(12.))
                        .text_color(color("text-tertiary"))
                        .child("· optional"),
                )
                .test_support(),
        )
        .child(
            div()
                .id("add-project-setup")
                .font_family(code_family())
                .text_size(scale::mono(12.))
                .child(Input::new(&picked.setup).aria_label("Setup command"))
                .test_support(),
        )
        .child(
            hint()
                .id("add-project-setup-help")
                .aria_label("Runs in each new worktree before the agent starts.")
                .child("Runs in each new worktree before the agent starts.")
                .test_support(),
        )
        .into_any_element()
}

#[cfg(test)]
mod tests {
    use crew_core::project_kind::{ProjectKind, WorkMode};
    use crew_proto::{SetupFile, SetupFileSize};

    use super::{
        file_note, inside_repo_note, kind_chip, mode_copy, repos_line, short_remote, size_label,
    };

    #[test]
    fn a_folder_inside_a_repo_names_the_repo() {
        assert_eq!(
            inside_repo_note("api"),
            "Inside the api repo, so Crew adds the whole repo."
        );
    }

    #[test]
    fn sizes_read_like_the_board() {
        assert_eq!(size_label(SetupFileSize::File { bytes: 412 }), "412 B");
        assert_eq!(size_label(SetupFileSize::File { bytes: 1024 }), "1 KB");
        assert_eq!(size_label(SetupFileSize::File { bytes: 1300 }), "1 KB");
        assert_eq!(size_label(SetupFileSize::File { bytes: 3 << 20 }), "3 MB");
        assert_eq!(size_label(SetupFileSize::Folder { files: 1 }), "1 file");
        assert_eq!(size_label(SetupFileSize::Folder { files: 4 }), "4 files");
    }

    #[test]
    fn secret_files_say_why_they_start_left_out() {
        let file = |path: &str| SetupFile {
            path: path.into(),
            size: SetupFileSize::File { bytes: 9 },
            ignored: true,
            looks_secret: true,
        };
        assert_eq!(
            file_note(&file(".env.production"), false),
            "live keys, left out"
        );
        assert_eq!(
            file_note(&file("certs/dev.pem"), false),
            "looks secret, left out"
        );
        assert_eq!(file_note(&file(".env.production"), true), "9 B");
    }

    #[test]
    fn remotes_drop_scheme_user_and_dot_git() {
        for url in [
            "git@github.com:owner/api.git",
            "https://github.com/owner/api.git",
            "ssh://git@github.com/owner/api",
            "https://user@github.com/owner/api/",
        ] {
            assert_eq!(short_remote(url), "github.com/owner/api", "{url}");
        }
        assert_eq!(short_remote("/work/demo.git"), "/work/demo");
    }

    #[test]
    fn many_repos_name_three_and_count_the_rest() {
        let repos = |names: &[&str]| names.iter().map(|n| n.to_string()).collect::<Vec<_>>();
        assert_eq!(
            repos_line(&repos(&["web", "api", "infra"])),
            "Folder with 3 git repos inside: api, infra, web"
        );
        let many: Vec<String> = (0..30).map(|i| format!("r{i:02}")).collect();
        assert_eq!(
            repos_line(&many),
            "Folder with 30 git repos inside: r00, r01, r02 and 27 more"
        );
    }

    #[test]
    fn chips_and_mode_copy_follow_the_kind() {
        assert_eq!(kind_chip(&ProjectKind::Repo), "git");
        assert_eq!(kind_chip(&ProjectKind::Plain), "folder");
        let multi = ProjectKind::MultiRepo {
            repos: vec!["a".into(), "b".into(), "c".into()],
        };
        assert_eq!(kind_chip(&multi), "3 repos");
        assert_eq!(
            mode_copy(&ProjectKind::Repo, WorkMode::OwnCopy).0,
            "Own worktree per thread (recommended)"
        );
        assert_eq!(
            mode_copy(&multi, WorkMode::OwnCopy).0,
            "Own copy per thread (recommended)"
        );
        assert_eq!(
            mode_copy(&ProjectKind::Plain, WorkMode::OwnCopy),
            (
                "Own copy per thread",
                "Needs at least one git repo in the folder."
            )
        );
    }
}
