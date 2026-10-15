//! Application-owned control hierarchy: commands, view switches and toggle states.
use gpui_kit::{
    component::{button::*, *},
    *,
};

pub fn chrome(cx: &App) -> Hsla {
    rgb(if cx.theme().is_dark() {
        0x24262b
    } else {
        0xf7f8fa
    })
    .into()
}

/// Quiet commands still inherit the button's press, focus and keyboard behavior.
pub fn quiet_button(id: impl Into<ElementId>, cx: &App) -> Button {
    Button::new(id).small().rounded(px(7.)).custom(
        ButtonCustomVariant::new(cx)
            .foreground(cx.theme().foreground)
            .hover(cx.theme().foreground.opacity(0.055))
            .active(cx.theme().foreground.opacity(0.10)),
    )
}

/// A persistent on/off state is a tint plus a stable icon, never a heavy outline.
pub fn toggle_button(id: impl Into<ElementId>, enabled: bool, cx: &App) -> Button {
    let tint = cx.theme().primary;
    quiet_button(id, cx).toggled(enabled).custom(
        ButtonCustomVariant::new(cx)
            .color(if enabled {
                tint.opacity(0.10)
            } else {
                cx.theme().transparent
            })
            .foreground(if enabled {
                tint
            } else {
                cx.theme().muted_foreground
            })
            .hover(if enabled {
                tint.opacity(0.16)
            } else {
                cx.theme().foreground.opacity(0.055)
            })
            .active(tint.opacity(0.21)),
    )
}

/// One material for analysis surfaces; spacing, rather than nested borders, separates them.
pub fn panel(cx: &App) -> Div {
    div()
        .bg(cx.theme().background)
        .border_1()
        .border_color(cx.theme().border.opacity(0.7))
        .rounded(px(14.))
        .overflow_hidden()
}

pub fn metric(icon: IconName, label: &str, value: String, color: Hsla, cx: &App) -> Div {
    let (figure, unit) = value
        .rsplit_once(' ')
        .map(|(figure, unit)| (figure.to_owned(), unit.to_owned()))
        .unwrap_or((value, String::new()));
    div()
        .flex_1()
        .min_w_0()
        .flex()
        .items_center()
        .gap_3()
        .child(
            div()
                .size(px(34.))
                .flex_shrink_0()
                .rounded_full()
                .bg(color.opacity(0.10))
                .text_color(color)
                .flex()
                .items_center()
                .justify_center()
                .child(Icon::new(icon).small()),
        )
        .child(
            div()
                .min_w_0()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().muted_foreground)
                        .child(label.to_owned()),
                )
                .child(
                    div()
                        .flex()
                        .items_baseline()
                        .gap_1p5()
                        .child(
                            div()
                                .truncate()
                                .text_size(px(22.))
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(figure),
                        )
                        .child(
                            div()
                                .text_size(px(12.))
                                .text_color(cx.theme().muted_foreground)
                                .child(unit),
                        ),
                ),
        )
}
