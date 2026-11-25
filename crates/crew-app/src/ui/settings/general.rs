//! Settings › General: how new threads get their titles, where crewd
//! keeps its data and the editor Open in editor uses.

use crew_proto::{Request, TitleMode, TitleModel, TitleSettings};
use gpui_kit::component::IndexPath;
use gpui_kit::component::select::{Select, SelectEvent, SelectState};
use gpui_kit::*;

use super::{control, field, segment, segmented};
use crate::theme::color;
use crate::theme::scale::{self, ui_px, weight};
use crate::ui::workspace::Workspace;

/// How wide the page gets.
pub const PAGE_WIDTH: f32 = 720.;
/// The model select.
const MODEL_WIDTH: f32 = 220.;

/// The model select's items, in its order.
pub const MODELS: [(TitleModel, &str); 3] = [
    (TitleModel::Haiku, "Haiku"),
    (TitleModel::Sonnet, "Sonnet"),
    (TitleModel::CodexCheapest, "Codex, cheapest model"),
];

pub const CODEX_FALLBACK: &str = "When Claude isn't signed in, Crew uses Codex's cheapest model.";

/// What the title run costs, on the model picked.
pub fn cost_hint(model: TitleModel) -> String {
    let on = match model {
        TitleModel::Haiku => "Haiku",
        TitleModel::Sonnet => "Sonnet",
        TitleModel::CodexCheapest => "Codex's cheapest model",
    };
    format!("Generated titles use a short run on {on}. It counts toward your plan.")
}

pub type ModelSelect = SelectState<Vec<SharedString>>;

/// The page's controls, made when it first opens.
pub struct GeneralPage {
    pub model: Entity<ModelSelect>,
    _events: Subscription,
}

fn model_index(model: TitleModel) -> Option<IndexPath> {
    MODELS
        .iter()
        .position(|(m, _)| *m == model)
        .map(IndexPath::new)
}

/// A form row as the boards draw them: 16 above and below, a rule on top.
pub(super) fn board_row(row: Div) -> Div {
    row.py(scale::row(16.)).border_b_0().border_t_1()
}

impl Workspace {
    /// Makes the model select the first time, else shows crewd's model in
    /// it.
    pub(super) fn general_page_opened(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let model = self.state().titles().model;
        let shown = self
            .settings
            .as_ref()
            .and_then(|s| s.general.as_ref())
            .map(|g| g.model.clone());
        if let Some(select) = shown {
            select.update(cx, |select, cx| {
                select.set_selected_index(model_index(model), window, cx);
            });
            return;
        }
        let labels: Vec<SharedString> = MODELS.iter().map(|(_, l)| (*l).into()).collect();
        let select = cx.new(|cx| SelectState::new(labels, model_index(model), window, cx));
        let events = cx.subscribe_in(
            &select,
            window,
            |this, _, event: &SelectEvent<Vec<SharedString>>, window, cx| {
                if let SelectEvent::Confirm(Some(label)) = event {
                    let picked = MODELS.iter().find(|(_, l)| *l == label.as_ref());
                    if let Some((model, _)) = picked {
                        let titles = this.state().titles();
                        this.set_titles(
                            TitleSettings {
                                model: *model,
                                ..titles
                            },
                            window,
                            cx,
                        );
                    }
                }
            },
        );
        if let Some(settings) = self.settings.as_mut() {
            settings.general = Some(GeneralPage {
                model: select,
                _events: events,
            });
        }
    }

    /// Saves how new threads get their titles. It shows at once; crewd's
    /// refusal puts it back.
    pub(crate) fn set_titles(
        &mut self,
        titles: TitleSettings,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.state().titles() == titles {
            return;
        }
        self.state_mut().expect_titles(titles);
        let answer = self.client.request(Request::SetTitleSettings { titles });
        self.spawn_request(window, cx, async move |this, cx| {
            let Err(error) = answer.await else {
                return Ok(());
            };
            this.update(cx, |this, cx| {
                this.state_mut().drop_titles();
                cx.notify();
            })
            .ok();
            Err((error, None))
        });
        cx.notify();
    }

    /// The board's Thread titles row and the comparison under it.
    pub(super) fn render_title_fields(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let titles = self.state().titles();
        let modes: Vec<AnyElement> = [
            (TitleMode::Generated, "generated", "Generated"),
            (
                TitleMode::FirstLine,
                "first-line",
                "First line of the message",
            ),
        ]
        .into_iter()
        .map(|(mode, key, label)| {
            segment(format!("general-titles-{key}"), label, titles.mode == mode)
                .flex_none()
                .min_w(ui_px(36.))
                .text_size(scale::text(12.))
                .font_weight(weight(FontWeight::MEDIUM))
                .on_click(cx.listener(move |this, _, window, cx| {
                    let titles = this.state().titles();
                    this.set_titles(TitleSettings { mode, ..titles }, window, cx);
                }))
                .test_support()
                .into_any_element()
        })
        .collect();
        let hint = |id: &'static str, text: String| {
            div()
                .id(id)
                .text_size(scale::text(12.))
                .line_height(scale::line(18.))
                .text_color(color("text-tertiary"))
                .aria_label(text.clone())
                .child(text)
                .test_support()
        };
        let model = self
            .settings
            .as_ref()
            .and_then(|s| s.general.as_ref())
            .map(|g| g.model.clone());
        let titles_row = div()
            .id("general-row-titles")
            .aria_label("Thread titles")
            .child(board_row(field(
                "Thread titles",
                Some("Set after the first message in a thread."),
                div()
                    .flex()
                    .flex_col()
                    .gap(ui_px(10.))
                    .child(div().flex().child(segmented(modes)))
                    .child(hint("general-titles-cost", cost_hint(titles.model)))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(ui_px(8.))
                            .text_size(scale::text(12.))
                            .text_color(color("text-secondary"))
                            .child("Model")
                            .children(model.map(|model| {
                                div().w(ui_px(MODEL_WIDTH)).child(control(
                                    "general-titles-model",
                                    Select::new(&model).accessibility_label("Model"),
                                ))
                            })),
                    )
                    .child(hint("general-titles-fallback", CODEX_FALLBACK.into())),
            )))
            .test_support();

        let example = |label: &'static str, text: &'static str| {
            div()
                .flex()
                .gap(ui_px(4.))
                .min_w_0()
                .child(format!("{label}:"))
                .child(
                    div()
                        .min_w_0()
                        .text_color(color("text-primary"))
                        .child(text),
                )
        };
        let comparison = board_row(field(
            "First line, for comparison",
            Some("What the other option gives you."),
            div()
                .flex()
                .flex_col()
                .gap(ui_px(6.))
                .text_size(scale::text(12.))
                .text_color(color("text-secondary"))
                .child(example("Generated", "Flaky parallel runner on Linux"))
                .child(example(
                    "First line",
                    "can you look at why the parallel runner flakes on linux sometimes, saw it twi…",
                )),
        ));
        div().flex().flex_col().child(titles_row).child(comparison)
    }
}

#[cfg(test)]
mod tests {
    use crew_proto::TitleModel;

    use super::cost_hint;

    #[test]
    fn the_cost_hint_names_the_model_picked() {
        assert_eq!(
            cost_hint(TitleModel::Haiku),
            "Generated titles use a short run on Haiku. It counts toward your plan."
        );
        assert!(cost_hint(TitleModel::Sonnet).contains("on Sonnet."));
        assert!(cost_hint(TitleModel::CodexCheapest).contains("on Codex's cheapest model."));
    }
}
