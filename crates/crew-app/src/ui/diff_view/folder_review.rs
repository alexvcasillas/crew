//! The parts of Review changes only a folder project has: files by
//! repo, a commit per repo and files outside git.

use super::*;

/// The parts of a review only a folder project has.
pub(super) struct FolderReview {
    pub(super) mode: FolderMode,
    /// In the order crewd lists the repos, each with its files.
    pub(super) groups: Vec<RepoGroup>,
    /// Files the thread changed outside any repo.
    pub(super) outside_git: Vec<OutsideFile>,
    /// A commit message per repo, made as repos show up.
    pub(super) messages: Vec<RepoMessage>,
}

/// One repo's files in [`Review::files`].
#[derive(Debug, Clone, PartialEq)]
pub(super) struct RepoGroup {
    /// Relative to the project folder; empty for the folder itself.
    pub(super) repo: String,
    pub(super) branch: Option<String>,
    pub(super) files: Range<usize>,
    /// Files of the range, by path, the thread didn't change.
    pub(super) outside: HashSet<String>,
}

impl RepoGroup {
    /// The files a commit or discard takes: the thread's own.
    pub(super) fn own(&self, files: &[FileDiff]) -> usize {
        files[self.files.clone()]
            .iter()
            .filter(|f| !self.outside.contains(&f.path))
            .count()
    }
}

pub(super) struct RepoMessage {
    pub(super) repo: String,
    pub(super) input: Entity<TextareaState>,
    pub(super) _events: Subscription,
}

/// The files of `changes` repo by repo, with each repo's place in them.
/// Paths keep the repo in front, as the patch names them.
pub(super) fn group_files(changes: &ThreadChanges) -> (Vec<FileDiff>, Vec<RepoGroup>) {
    let mut files = Vec::new();
    let mut groups = Vec::new();
    for repo in changes.repos.iter().filter(|r| !r.diff.files.is_empty()) {
        let start = files.len();
        files.extend(parse(&repo.diff.patch));
        let outside = repo
            .outside_thread
            .iter()
            .map(|p| repo_path(&repo.repo, p))
            .collect();
        groups.push(RepoGroup {
            repo: repo.repo.clone(),
            branch: repo.branch.clone(),
            files: start..files.len(),
            outside,
        });
    }
    (files, groups)
}

/// `path` of `repo` as the project folder has it.
pub(super) fn repo_path(repo: &str, path: &str) -> String {
    if repo.is_empty() {
        path.to_string()
    } else {
        format!("{repo}/{path}")
    }
}

/// `path` without its repo in front.
pub(super) fn in_repo<'a>(repo: &str, path: &'a str) -> &'a str {
    if repo.is_empty() {
        return path;
    }
    path.strip_prefix(repo)
        .and_then(|rest| rest.strip_prefix('/'))
        .unwrap_or(path)
}

impl Workspace {
    /// Makes a commit message for each repo that shows up in a folder
    /// review: the one you left there, else the draft.
    pub(super) fn ensure_repo_messages(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(review) = &self.review else {
            return;
        };
        let Some(folder) = &review.folder else {
            return;
        };
        let missing: Vec<String> = folder
            .groups
            .iter()
            .map(|g| g.repo.clone())
            .filter(|repo| !folder.messages.iter().any(|m| &m.repo == repo))
            .collect();
        if missing.is_empty() {
            return;
        }
        let (thread, draft) = (review.thread, review.draft.clone());
        let mut made = Vec::new();
        for repo in missing {
            let text = match self.repo_messages.remove(&(thread, repo.clone())) {
                Some(saved) if saved.text != saved.draft && !saved.text.is_empty() => saved.text,
                _ => draft.clone(),
            };
            let input = cx.new(|cx| {
                TextareaState::new(window, cx)
                    .rows(3)
                    .placeholder("Say what changed and why.")
                    .default_value(text)
            });
            let events = cx.subscribe(&input, |_, _, event: &InputEvent, cx| {
                if matches!(event, InputEvent::Change) {
                    cx.notify();
                }
            });
            made.push(RepoMessage {
                repo,
                input,
                _events: events,
            });
        }
        if let Some(folder) = self.review.as_mut().and_then(|r| r.folder.as_mut()) {
            folder.messages.extend(made);
        }
        cx.notify();
    }

    /// The commit message of a folder review's group `ix`.
    #[cfg(test)]
    pub(in crate::ui) fn review_repo_message(&self, ix: usize, cx: &App) -> Option<String> {
        let folder = self.review.as_ref()?.folder.as_ref()?;
        let repo = &folder.groups.get(ix)?.repo;
        folder
            .messages
            .iter()
            .find(|m| &m.repo == repo)
            .map(|m| m.input.read(cx).value().to_string())
    }

    /// The repos a folder review's discard takes: those with files the
    /// thread changed.
    pub(super) fn discard_repos(&self) -> Vec<String> {
        let Some(review) = &self.review else {
            return Vec::new();
        };
        let Some(folder) = &review.folder else {
            return Vec::new();
        };
        folder
            .groups
            .iter()
            .filter(|g| g.own(&review.files) > 0)
            .map(|g| g.repo.clone())
            .collect()
    }

    /// How a repo is named: its folder, or the project's for the folder
    /// itself.
    pub(super) fn repo_name(&self, thread: ThreadId, repo: &str) -> String {
        if !repo.is_empty() {
            return repo.to_string();
        }
        let state = self.state();
        state
            .thread_project(thread)
            .and_then(|p| state.project(p))
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// Whether the commit of group `ix` can be pressed.
    pub(super) fn repo_can_commit(&self, ix: usize, cx: &App) -> bool {
        let Some(review) = &self.review else {
            return false;
        };
        let Some(folder) = &review.folder else {
            return false;
        };
        let Some(group) = folder.groups.get(ix) else {
            return false;
        };
        let message = folder.messages.iter().find(|m| m.repo == group.repo);
        !review.busy
            && group.own(&review.files) > 0
            && message.is_some_and(|m| !m.input.read(cx).value().trim().is_empty())
    }

    /// Commits the thread's files of group `ix`'s repo.
    pub(super) fn commit_repo(&mut self, ix: usize, window: &mut Window, cx: &mut Context<Self>) {
        if !self.repo_can_commit(ix, cx) {
            return;
        }
        let Some(review) = &mut self.review else {
            return;
        };
        let Some(folder) = &review.folder else {
            return;
        };
        let group = folder.groups[ix].clone();
        let Some(input) = folder
            .messages
            .iter()
            .find(|m| m.repo == group.repo)
            .map(|m| m.input.clone())
        else {
            return;
        };
        review.busy = true;
        let thread = review.thread;
        let paths: Vec<String> = review.files[group.files.clone()]
            .iter()
            .map(|f| f.path.clone())
            .collect();
        let message = input.read(cx).value().to_string();
        let name = self.repo_name(thread, &group.repo);
        let branch = group.branch.clone().unwrap_or_default();
        let answer = self.client.request(Request::Commit {
            thread,
            repo: (!group.repo.is_empty()).then(|| group.repo.clone()),
            message,
        });
        self.spawn_request(window, cx, async move |this, cx| {
            let result = answer.await;
            this.update_in(cx, |this, window, cx| {
                if let Some(review) = &mut this.review {
                    review.busy = false;
                }
                if result.is_ok() {
                    window.push_notification(
                        Notification::success(format!("Committed to {branch} in {name}.")),
                        cx,
                    );
                    if let Some(viewed) = this.viewed.get_mut(&thread) {
                        viewed.retain(|p| !paths.contains(p));
                    }
                    input.update(cx, |input, cx| input.set_value("", window, cx));
                    this.load_review(cx);
                }
                cx.notify();
            })
            .ok();
            result.map(drop).map_err(|e| (e, None))
        });
        cx.notify();
    }

    /// A folder review's file list: the count, each repo's files, then
    /// the files git doesn't have.
    pub(super) fn render_folder_files(&self, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let Some(review) = &self.review else {
            return Vec::new();
        };
        let Some(folder) = &review.folder else {
            return Vec::new();
        };
        let viewed = self.viewed.get(&review.thread);
        let added: u32 = review.files.iter().map(|f| f.added).sum();
        let removed: u32 = review.files.iter().map(|f| f.removed).sum();
        let n = review.files.len() + folder.outside_git.len();
        let repos = folder.groups.len();
        let count = match repos {
            0 => files_count(n),
            1 => format!("{} in 1 repo", files_count(n)),
            _ => format!("{} in {repos} repos", files_count(n)),
        };
        let mut rows = vec![
            div()
                .id("review-summary")
                .flex()
                .items_center()
                .justify_between()
                .pt(scale::row(12.))
                .pb(scale::row(4.))
                .px(ui_px(16.))
                .aria_label(format!("{count} · +{added} −{removed}"))
                .child(
                    div()
                        .text_size(scale::text(12.))
                        .font_weight(weight(FontWeight::MEDIUM))
                        .text_color(color("text-secondary"))
                        .child(count),
                )
                .child(plus_minus(added, removed))
                .test_support()
                .into_any_element(),
        ];
        for (gi, group) in folder.groups.iter().enumerate() {
            let name = self.repo_name(review.thread, &group.repo);
            let files = files_count(group.files.len());
            rows.push(
                group_header(("review-repo", gi), BRANCH_ICON, "text-primary")
                    .aria_label(format!("{name} · {files}"))
                    .child(div().font_family(code_family()).child(name))
                    .child(
                        div()
                            .font_weight(weight(FontWeight::NORMAL))
                            .text_color(color("text-tertiary"))
                            .child(format!("· {files}")),
                    )
                    .test_support()
                    .into_any_element(),
            );
            let file_rows = group.files.clone().map(|ix| {
                let file = &review.files[ix];
                let path = in_repo(&group.repo, &file.path).to_string();
                let outside = group.outside.contains(&file.path);
                let seen = viewed.is_some_and(|v| v.contains(&file.path));
                let current = review.current_file.as_ref() == Some(&file.path);
                let mut label = format!(
                    "{} {path} +{} −{}",
                    file.kind.letter(),
                    file.added,
                    file.removed
                );
                if outside {
                    label.push_str(&format!(" · {OUTSIDE_THREAD}"));
                }
                let text = if current {
                    "text-primary"
                } else if seen || outside {
                    "text-tertiary"
                } else {
                    "text-secondary"
                };
                div()
                    .id(("review-file", ix))
                    .flex()
                    .items_center()
                    .gap(ui_px(8.))
                    .h(scale::row(28.))
                    .px(ui_px(8.))
                    .rounded(ui_px(6.))
                    .cursor_pointer()
                    .when(current, |row| row.bg(color("surface-300")))
                    .when(!current, |row| row.hover(|s| s.bg(color("surface-100"))))
                    .font_family(code_family())
                    .whitespace_nowrap()
                    .aria_label(label)
                    .child(
                        div()
                            .flex_none()
                            .w(scale::mono(12.))
                            .text_size(scale::mono(10.))
                            .font_weight(weight(FontWeight::MEDIUM))
                            .text_color(color(letter_colour(&file.kind)))
                            .child(file.kind.letter().to_string()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_size(scale::mono(12.))
                            .text_color(color(text))
                            .child(path),
                    )
                    .when(seen, |row| row.child(dot("text-tertiary")))
                    .child(short_plus_minus(file.added, file.removed))
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.scroll_to_file(ix, cx);
                    }))
                    .test_support()
                    .into_any_element()
            });
            rows.push(group_rows(file_rows.collect()));
        }
        if !folder.outside_git.is_empty() {
            let files = files_count(folder.outside_git.len());
            rows.push(
                group_header("review-not-in-git", FILE_ICON, "text-secondary")
                    .aria_label(format!("Not in git · {files}"))
                    .child("Not in git")
                    .child(
                        div()
                            .font_weight(weight(FontWeight::NORMAL))
                            .text_color(color("text-tertiary"))
                            .child(format!("· {files}")),
                    )
                    .test_support()
                    .into_any_element(),
            );
            let mut outside: Vec<AnyElement> = folder
                .outside_git
                .iter()
                .enumerate()
                .map(|(ix, file)| {
                    let what = if file.exists { "edited" } else { "removed" };
                    div()
                        .id(("review-outside", ix))
                        .flex()
                        .items_center()
                        .gap(ui_px(8.))
                        .h(scale::row(28.))
                        .px(ui_px(8.))
                        .whitespace_nowrap()
                        .text_color(color("text-secondary"))
                        .aria_label(format!("{} {what}", file.path))
                        .child(
                            div()
                                .flex_none()
                                .w(scale::mono(12.))
                                .font_family(code_family())
                                .text_size(scale::mono(10.))
                                .font_weight(weight(FontWeight::MEDIUM))
                                .text_color(color("text-tertiary"))
                                .child("·"),
                        )
                        .child(
                            div()
                                .flex_1()
                                .min_w_0()
                                .truncate()
                                .font_family(code_family())
                                .text_size(scale::mono(12.))
                                .child(file.path.clone()),
                        )
                        .child(
                            div()
                                .text_size(scale::text(11.))
                                .text_color(color("text-tertiary"))
                                .child(what),
                        )
                        .test_support()
                        .into_any_element()
                })
                .collect();
            outside.push(
                div()
                    .id("review-outside-note")
                    .pt(scale::row(2.))
                    .pl(ui_px(28.))
                    .pr(ui_px(8.))
                    .text_size(scale::text(12.))
                    .line_height(scale::line(18.))
                    .text_color(color("text-tertiary"))
                    .aria_label(NOT_IN_GIT)
                    .child(NOT_IN_GIT)
                    .test_support()
                    .into_any_element(),
            );
            rows.push(group_rows(outside));
        }
        rows
    }

    /// What next for a folder project: a commit per repo, what git can't
    /// undo, then asking for changes or discarding them.
    pub(super) fn render_folder_next(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(review) = &self.review else {
            return div().into_any_element();
        };
        let Some(folder) = &review.folder else {
            return div().into_any_element();
        };
        let thread = review.thread;
        let agent = self.review_agent(thread);
        let mut primary_used = false;
        let mut panels = Vec::new();
        for (gi, group) in folder.groups.iter().enumerate() {
            let own = group.own(&review.files);
            if own == 0 {
                continue;
            }
            let Some(message) = folder.messages.iter().find(|m| m.repo == group.repo) else {
                continue;
            };
            let name = self.repo_name(thread, &group.repo);
            let branch = group.branch.clone().unwrap_or_default();
            let files = files_count(own);
            let can = self.repo_can_commit(gi, cx);
            let primary = can && !primary_used;
            primary_used |= primary;
            let drafted = !review.draft.is_empty()
                && message.input.read(cx).value().as_ref() == review.draft.as_str();
            let caption = match &agent {
                Some(agent) if drafted => format!("Commit message · drafted by {agent}"),
                _ => "Commit message".to_string(),
            };
            let label = format!("Commit {files} in {name}");
            let button = Button::new(("review-commit-repo", gi))
                .w_full()
                .label(label)
                .disabled(!can)
                .on_click(cx.listener(move |this, _, window, cx| this.commit_repo(gi, window, cx)));
            let input = message.input.clone();
            panels.push(
                div()
                    .id(("review-repo-panel", gi))
                    .flex()
                    .flex_col()
                    .gap(ui_px(8.))
                    .p(ui_px(12.))
                    .border_1()
                    .border_color(color("border-100"))
                    .rounded(ui_px(8.))
                    .child(
                        div()
                            .id(("review-repo-head", gi))
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .aria_label(format!("{name} on {branch} · {files}"))
                            .child(
                                div()
                                    .font_family(code_family())
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .child(name),
                            )
                            .child(
                                div()
                                    .flex()
                                    .gap(ui_px(4.))
                                    .min_w_0()
                                    .text_size(scale::text(12.))
                                    .text_color(color("text-tertiary"))
                                    .child("on")
                                    .child(
                                        div()
                                            .min_w_0()
                                            .truncate()
                                            .font_family(code_family())
                                            .text_size(scale::mono(12.))
                                            .child(branch),
                                    )
                                    .child(format!("· {files}")),
                            )
                            .test_support(),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(6.))
                            .child(
                                div()
                                    .id(("review-repo-caption", gi))
                                    .text_size(scale::text(12.))
                                    .font_weight(weight(FontWeight::MEDIUM))
                                    .text_color(color("text-secondary"))
                                    .aria_label(caption.clone())
                                    .child(caption)
                                    .test_support(),
                            )
                            .child(
                                div()
                                    .id(("review-repo-message", gi))
                                    .key_context(MESSAGE_CONTEXT)
                                    .on_action(cx.listener(|this, _: &LeaveMessage, window, cx| {
                                        if let Some(review) = &this.review {
                                            review.focus.focus(window, cx);
                                        }
                                    }))
                                    .font_family(code_family())
                                    .child(Textarea::new(&input).aria_label("Commit message"))
                                    .on_click(cx.listener(move |_, _, window, cx| {
                                        input.update(cx, |m, cx| m.focus(window, cx));
                                    }))
                                    .test_support(),
                            ),
                    )
                    .child(if primary {
                        button.primary()
                    } else {
                        button.outline()
                    })
                    .into_any_element(),
            );
        }
        let outside: Vec<String> = folder.outside_git.iter().map(|f| f.path.clone()).collect();
        let warning = match outside.as_slice() {
            [] => None,
            [one] => Some(format!(
                "{one} is not in git. It's already changed on disk and Crew can't undo it."
            )),
            many => Some(format!(
                "{} are not in git. They're already changed on disk and Crew can't undo them.",
                and_list(many)
            )),
        };
        let repos: Vec<String> = self
            .discard_repos()
            .iter()
            .map(|r| self.repo_name(thread, r))
            .collect();
        let discard = if repos.is_empty() {
            "Discard changes".to_string()
        } else {
            format!("Discard changes in {}", and_list(&repos))
        };
        let note = if folder.mode == FolderMode::InPlace {
            IN_PLACE_NOTE
        } else {
            NOTE
        };
        div()
            .id("review-next")
            .flex()
            .flex_col()
            .flex_none()
            .w(scale::chrome(INSPECTOR_WIDTH))
            .h_full()
            .border_l_1()
            .border_color(color("border-100"))
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .h(scale::row(44.))
                    .px(ui_px(16.))
                    .border_b_1()
                    .border_color(color("border-100"))
                    .text_size(scale::text(14.))
                    .font_weight(weight(FontWeight::SEMIBOLD))
                    .child("What next"),
            )
            .child(
                div()
                    .id("review-next-body")
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_h_0()
                    .gap(ui_px(16.))
                    .p(ui_px(16.))
                    .overflow_y_scroll()
                    .when_some(review.left.clone(), |col, (agent, n)| {
                        let text = format!(
                            "{agent} left {n} {}.",
                            if n == 1 { "comment" } else { "comments" }
                        );
                        col.child(
                            div()
                                .id("review-callout")
                                .px(ui_px(12.))
                                .py(scale::row(10.))
                                .bg(color("surface-100"))
                                .border_1()
                                .border_color(color("border-200"))
                                .rounded(ui_px(8.))
                                .text_size(scale::text(13.))
                                .text_color(color("text-primary"))
                                .aria_label(text.clone())
                                .child(text)
                                .test_support(),
                        )
                    })
                    .children(panels)
                    .when_some(warning, |col, text| {
                        col.child(warning_note("review-outside-warning", text))
                    })
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .gap(ui_px(8.))
                            .child(
                                Button::new("review-ask")
                                    .outline()
                                    .w_full()
                                    .label(agent.as_ref().map_or_else(
                                        || "Ask for changes".to_string(),
                                        |a| format!("Ask {a} for changes"),
                                    ))
                                    .disabled(agent.is_none())
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.ask_for_changes(window, cx)
                                    })),
                            )
                            .child(
                                Button::new("review-discard")
                                    .ghost()
                                    .w_full()
                                    .text_color(color("danger"))
                                    .label(discard)
                                    .disabled(repos.is_empty() || review.busy)
                                    .on_click(cx.listener(|this, _, window, cx| {
                                        this.ask_to_discard(window, cx)
                                    })),
                            ),
                    )
                    .child(
                        div()
                            .id("review-note")
                            .text_size(scale::text(12.))
                            .line_height(scale::line(18.))
                            .text_color(color("text-tertiary"))
                            .aria_label(note)
                            .child(note)
                            .test_support(),
                    ),
            )
            .into_any_element()
    }
}

/// A group's title in a folder review's file list.
fn group_header(id: impl Into<ElementId>, icon: &'static [u8], token: &str) -> Stateful<Div> {
    div()
        .id(id)
        .flex()
        .items_center()
        .gap(ui_px(6.))
        .h(scale::row(28.))
        .px(ui_px(16.))
        .mt(ui_px(8.))
        .text_size(scale::text(12.))
        .font_weight(weight(FontWeight::MEDIUM))
        .text_color(color(token))
        .child(small_icon(icon, "text-secondary"))
}

/// The rows under a group's title.
fn group_rows(rows: Vec<AnyElement>) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(ui_px(1.))
        .px(ui_px(8.))
        .children(rows)
        .into_any_element()
}

/// `+n −m` at 11 px, leaving out a side that is zero.
fn short_plus_minus(added: u32, removed: u32) -> impl IntoElement {
    div()
        .flex()
        .flex_none()
        .gap(ui_px(4.))
        .font_family(code_family())
        .text_size(scale::mono(11.))
        .when(added > 0, |row| {
            row.child(
                div()
                    .text_color(color("success"))
                    .child(format!("+{added}")),
            )
        })
        .when(removed > 0, |row| {
            row.child(
                div()
                    .text_color(color("danger"))
                    .child(format!("−{removed}")),
            )
        })
}
