//! Contextual formatting choices for the last paste.

use crate::{WordApp, icons, theme::Tokens};
use egui::{Event, Key, Pos2, Rect, Vec2};
use serde_json::{Value, json};

const OPTIONS: [(&str, &str, &str); 3] =
    [("source", "Keep Source Formatting", "paste_source"), ("merge", "Merge Formatting", "paste_merge"), ("text", "Keep Text Only", "paste_text")];

#[derive(Default)]
pub struct State {
    pub open: bool,
    focus: usize,
    ctrl_down: bool,
    chord: bool,
    pub(crate) focus_pending: bool,
    focus_ids: Vec<egui::Id>,
    rect: Option<Rect>,
}

impl State {
    pub(crate) fn focus_selected(&mut self, mode: &str) {
        self.focus = OPTIONS.iter().position(|o| o.0 == mode).unwrap_or(0);
        self.focus_pending = true;
    }
    fn ctrl_tap(&mut self, events: &[Event]) -> bool {
        let mut tap = false;
        for event in events {
            match event {
                Event::Key { key: Key::ControlLeft | Key::ControlRight, pressed, repeat, .. } => {
                    if *pressed && !repeat {
                        self.ctrl_down = true;
                        self.chord = false;
                    } else if !pressed {
                        tap |= self.ctrl_down && !self.chord;
                        self.ctrl_down = false;
                    }
                }
                Event::Key { pressed: true, .. }
                | Event::Text(_)
                | Event::Paste(_)
                | Event::Copy
                | Event::Cut
                | Event::PointerButton { pressed: true, .. }
                    if self.ctrl_down =>
                {
                    self.chord = true;
                }
                _ => {}
            }
        }
        tap
    }
}

fn anchor_rect(caret: Pos2, height: f32, size: Vec2, viewport: Rect) -> Option<Rect> {
    if !caret.is_finite()
        || !height.is_finite()
        || height < 0.0
        || !size.is_finite()
        || !viewport.is_finite()
        || !viewport.is_positive()
        || size.x > viewport.width()
        || size.y > viewport.height()
    {
        return None;
    }
    let x = (caret.x + 6.0).clamp(viewport.left(), viewport.right() - size.x);
    let below = caret.y + height + 4.0;
    let y = if below + size.y <= viewport.bottom() { below } else { caret.y - size.y - 4.0 };
    Some(Rect::from_min_size(egui::pos2(x, y.clamp(viewport.top(), viewport.bottom() - size.y)), size))
}

/// Enrich a user paste only when the host snapshot belongs to the same clipboard text.
pub fn clipboard_params(app: &WordApp, id: &str, params: &mut Value) {
    if !matches!(id, "edit.paste" | "edit.pasteMerge" | "edit.pasteText" | "edit.pasteSpecial")
        || params.get("html").is_some()
        || params.get("rtf").is_some()
    {
        return;
    }
    let Some(snapshot) = app.services.clipboard_rich.as_ref().and_then(|f| f()) else { return };
    if let Some(text) = params.get("text").and_then(Value::as_str)
        && snapshot.get("text").and_then(Value::as_str).is_none_or(|t| t.replace("\r\n", "\n") != text.replace("\r\n", "\n"))
    {
        return;
    }
    if let (Some(to), Some(from)) = (params.as_object_mut(), snapshot.as_object()) {
        for key in ["text", "html", "rtf"] {
            if let Some(value) = from.get(key).filter(|v| v.as_str().is_some_and(|t| !t.is_empty())) {
                to.entry(key.to_string()).or_insert_with(|| value.clone());
            }
        }
    }
}

fn visible(app: &WordApp) -> bool {
    app.session.paste_options_valid()
        && app.dialog.is_none()
        && app.colors.is_none()
        && !app.ui.backstage
        && !app.canvas.context_menu_open
        && !app.canvas.drag_selecting()
        && app.ui.keytips == crate::keytips::Phase::Off
}

/// Read menu keys before the canvas, so choosing an option never types or moves the caret.
pub fn keyboard(app: &mut WordApp, ctx: &egui::Context) {
    let events = ctx.input(|i| i.events.clone());
    let tap = app.paste_options.ctrl_tap(&events);
    if !app.session.paste_options_valid() {
        app.session.last_paste = None;
    }
    if !visible(app) {
        app.paste_options.open = false;
        return;
    }
    let popup_focused = ctx.memory(|m| m.focused()).is_some_and(|id| app.paste_options.focus_ids.contains(&id));
    if app.paste_options.open
        && events
            .iter()
            .any(|event| matches!(event, Event::PointerButton {pos, pressed:true, ..} if app.paste_options.rect.is_none_or(|r| !r.contains(*pos))))
    {
        app.paste_options.open = false;
        app.paste_options.focus_pending = false;
        return;
    }
    if !app.canvas.focused && !popup_focused && !app.paste_options.focus_pending {
        app.paste_options.open = false;
        return;
    }
    if tap && (app.canvas.focused || popup_focused) {
        let _ = app.run("ui.pasteOptions", json!({}));
    }
    let choosing_space = events.iter().any(|e| matches!(e, Event::Key {key:Key::Space, pressed:true, modifiers, ..} if modifiers.is_none()));
    if (app.paste_options.open || popup_focused)
        && events.iter().any(|event| match event {
            Event::Text(text) => !(choosing_space && text == " "),
            Event::Paste(_) | Event::Copy | Event::Cut | Event::Ime(_) => true,
            Event::Key { key, pressed: true, modifiers, .. } => {
                if matches!(key, Key::ControlLeft | Key::ControlRight | Key::ShiftLeft | Key::ShiftRight) {
                    false
                } else if !modifiers.is_none() && !(*key == Key::Tab && modifiers.shift_only()) {
                    true
                } else {
                    !matches!(
                        key,
                        Key::ArrowLeft | Key::ArrowRight | Key::ArrowUp | Key::ArrowDown | Key::Tab | Key::Enter | Key::Space | Key::Escape
                    )
                }
            }
            _ => false,
        })
    {
        app.paste_options.open = false;
        app.paste_options.focus_pending = false;
        app.canvas.want_focus = true;
        return;
    }
    let mut handled = Vec::new();
    let mut mode = None;
    for event in events {
        if let Event::Key { key, pressed: true, modifiers, .. } = event {
            if !modifiers.is_none() && !(key == Key::Tab && modifiers.shift_only()) {
                continue;
            }
            if key == Key::Escape {
                app.canvas.want_focus = true;
                let _ = app.run("ui.dismissPasteOptions", json!({}));
                handled.push(key);
            } else if app.paste_options.open {
                match key {
                    Key::ArrowLeft | Key::ArrowUp => app.paste_options.focus = (app.paste_options.focus + 2) % 3,
                    Key::Tab if modifiers.shift => app.paste_options.focus = (app.paste_options.focus + 2) % 3,
                    Key::ArrowRight | Key::ArrowDown | Key::Tab => app.paste_options.focus = (app.paste_options.focus + 1) % 3,
                    Key::Enter | Key::Space => mode = OPTIONS.get(app.paste_options.focus).map(|o| o.0),
                    _ => continue,
                }
                handled.push(key);
                if app.session.last_paste.as_ref().is_none_or(|p| !p.rich) {
                    app.paste_options.focus = 2;
                }
                app.paste_options.focus_pending = true;
            }
        }
    }
    if let Some(mode) = mode {
        choose(app, mode);
    }
    ctx.input_mut(|i| {
        i.events.retain(|e| {
            !matches!(e, Event::Key { key, .. } if handled.contains(key))
                && !(handled.contains(&Key::Space) && matches!(e, Event::Text(text) if text == " "))
        })
    });
}

fn choose(app: &mut WordApp, mode: &str) {
    if mode != "text" && app.session.last_paste.as_ref().is_none_or(|p| !p.rich) {
        return;
    }
    if app.run("edit.pasteOptions", json!({"mode": mode})).is_ok() {
        app.paste_options.open = false;
        app.paste_options.focus_pending = false;
        app.canvas.focused = true;
        app.canvas.want_focus = true;
    }
}

fn hide_menu(app: &mut WordApp, ctx: &egui::Context) {
    if app.paste_options.open {
        app.paste_options.open = false;
        app.paste_options.focus_pending = false;
        app.canvas.want_focus = true;
        ctx.request_repaint();
    }
}

pub fn show(app: &mut WordApp, ctx: &egui::Context, viewport: Rect) {
    if !visible(app) {
        app.paste_options.open = false;
        return;
    }
    let Some((caret, height)) = crate::canvas::caret_screen(app) else {
        hide_menu(app, ctx);
        return;
    };
    // Do not detach the widget from pasted content that has scrolled off screen.
    if !viewport.contains(caret) {
        hide_menu(app, ctx);
        return;
    }
    let open = app.paste_options.open;
    let size = egui::vec2(92.0, 30.0);
    let Some(rect) = anchor_rect(caret, height, size, viewport) else {
        hide_menu(app, ctx);
        return;
    };
    let t = Tokens::get(ctx);
    let mode = app.session.last_paste.as_ref().map(|p| p.mode).unwrap_or("source");
    let rich = app.session.last_paste.as_ref().is_some_and(|p| p.rich);
    app.paste_options.focus_ids.clear();
    let button = egui::Area::new(egui::Id::new("wc_paste_options"))
        .order(egui::Order::Foreground)
        .fixed_pos(rect.min)
        .movable(false)
        .constrain_to(viewport)
        .show(ctx, |ui| {
            ui.visuals_mut().widgets.inactive.corner_radius = egui::CornerRadius::ZERO;
            let button = crate::widgets::icon_text_button(ui, "paste", "(Ctrl)  ", crate::theme::regular(11.5));
            icons::paint(
                ui.painter(),
                Rect::from_center_size(egui::pos2(button.rect.right() - 6.0, button.rect.center().y), egui::vec2(9.0, 9.0)),
                "dropdown",
                t.icon,
                t.accent,
            );
            button.widget_info(|| egui::WidgetInfo::labeled(egui::WidgetType::Button, true, crate::tl!("Paste Options")));
            app.paste_options.focus_ids.push(button.id);
            let button_rect = button.rect;
            if button.on_hover_text(crate::tl!("Paste Options")).clicked() {
                let _ = app.run("ui.pasteOptions", json!({"value": !open}));
            }
            button_rect
        })
        .inner;
    app.paste_options.rect = Some(button);
    if app.paste_options.open {
        let menu_id = egui::Id::new("wc_paste_options_menu");
        let menu_size = egui::containers::AreaState::load(ctx, menu_id).and_then(|s| s.size).unwrap_or(egui::vec2(110.0, 54.0));
        let below = button.bottom() + 0.0 + menu_size.y <= viewport.bottom();
        let position = egui::pos2(button.left(), if below { button.bottom() + 0.0 } else { button.top() - 0.0 });
        let menu = egui::Area::new(menu_id)
            .order(egui::Order::Foreground)
            .pivot(if below { egui::Align2::LEFT_TOP } else { egui::Align2::LEFT_BOTTOM })
            .fixed_pos(position)
            .default_size(menu_size)
            .movable(false)
            .constrain_to(viewport)
            .show(ctx, |ui| {
                egui::Frame::new()
                    .fill(t.menu)
                    .stroke(egui::Stroke::new(1.0, t.border_strong))
                    .corner_radius(4)
                    .inner_margin(egui::Margin {
                        left: 2,
                        right: 20,
                        top: 2,
                        bottom: 2,
                    })
                    .show(ui, |ui| {
                        ui.label(egui::RichText::new(crate::tl!("Paste Options")).font(crate::theme::semibold(12.0)));
                        ui.horizontal(|ui| {
                            ui.spacing_mut().item_spacing.x = 1.0;
                            for (i, (value, label, icon)) in OPTIONS.into_iter().enumerate() {
                                let enabled = rich || value == "text";
                                ui.add_enabled_ui(enabled, |ui| {
                                    let (r, response) = ui.allocate_exact_size(egui::vec2(26.0, 26.0), egui::Sense::click());
                                    app.paste_options.focus_ids.push(response.id);
                                    crate::widgets::bg(ui, r, &response, mode == value, &t);
                                    let color = if enabled { t.icon } else { t.text_disabled };
                                    icons::paint(
                                        ui.painter(),
                                        Rect::from_center_size(r.center(), egui::vec2(23.0, 23.0)),
                                        icon,
                                        color,
                                        if enabled { t.accent } else { color },
                                    );
                                    response.widget_info(|| {
                                        egui::WidgetInfo::selected(egui::WidgetType::SelectableLabel, enabled, mode == value, crate::tl!(label))
                                    });
                                    if app.paste_options.focus == i {
                                        if app.paste_options.focus_pending && enabled {
                                            response.request_focus();
                                            app.paste_options.focus_pending = false;
                                        }
                                        ui.painter().rect_stroke(r.shrink(2.0), 3, egui::Stroke::new(1.0, t.accent), egui::StrokeKind::Inside);
                                    }
                                    if response.has_focus() {
                                        ui.memory_mut(|m| {
                                            m.set_focus_lock_filter(
                                                response.id,
                                                egui::EventFilter { tab: true, horizontal_arrows: true, vertical_arrows: true, escape: true },
                                            )
                                        });
                                    }
                                    if response.on_hover_text(crate::tl!(label)).clicked() {
                                        choose(app, value);
                                    }
                                });
                            }
                        });
                    })
                    .response
                    .rect
            })
            .inner;
        app.paste_options.rect = Some(button.union(menu));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(key: Key, pressed: bool) -> Event {
        Event::Key { key, physical_key: None, pressed, repeat: false, modifiers: egui::Modifiers::NONE }
    }

    #[test]
    fn ctrl_tap_opens_but_a_shortcut_does_not() {
        let mut state = State::default();
        assert!(!state.ctrl_tap(&[key(Key::ControlLeft, true)]));
        assert!(state.ctrl_tap(&[key(Key::ControlLeft, false)]));
        assert!(!state.ctrl_tap(&[key(Key::ControlLeft, true), key(Key::V, true), key(Key::ControlLeft, false)]));
        assert!(!state.ctrl_tap(&[key(Key::ControlRight, true), Event::Copy, key(Key::ControlRight, false)]));
        assert!(state.ctrl_tap(&[key(Key::ControlRight, true), key(Key::ControlRight, false)]));
    }

    #[test]
    fn popup_stays_inside_viewport_and_rejects_invalid_geometry() {
        let viewport = Rect::from_min_size(Pos2::ZERO, egui::vec2(400.0, 200.0));
        let size = egui::vec2(150.0, 100.0);
        let rect = anchor_rect(egui::pos2(398.0, 198.0), 20.0, size, viewport).unwrap();
        assert!(viewport.contains_rect(rect));
        assert!(rect.bottom() <= 198.0);
        assert!(anchor_rect(egui::pos2(f32::NAN, 0.0), 10.0, size, viewport).is_none());
    }

    fn app() -> WordApp {
        WordApp::new(wordcraft_engine::Session::new(wordcraft_doc::Document::new()), crate::Services::default())
    }

    #[test]
    fn clipboard_enrichment_rejects_a_changed_clipboard() {
        let mut app = app();
        app.services.clipboard_rich = Some(Box::new(|| Some(json!({"text":"new", "html":"<b>new</b>"}))));
        let mut params = json!({"text":"old"});
        clipboard_params(&app, "edit.paste", &mut params);
        assert_eq!(params, json!({"text":"old"}));
        let mut params = json!({"text":"new"});
        clipboard_params(&app, "edit.paste", &mut params);
        assert_eq!(params["html"], "<b>new</b>");
        clipboard_params(&app, "edit.pasteOptions", &mut params);
        assert_eq!(params["text"], "new");
    }

    #[test]
    fn menu_keys_choose_without_typing_and_escape_dismisses() {
        let mut app = app();
        app.run("edit.paste", json!({"text":"rich", "html":"<p><b>rich</b></p>"})).unwrap();
        app.run("ui.pasteOptions", json!({})).unwrap();
        let ctx = egui::Context::default();
        let input =
            egui::RawInput { events: vec![key(Key::ArrowRight, true), key(Key::ArrowRight, true), key(Key::Enter, true)], ..Default::default() };
        ctx.run_ui(input, |ui| {
            keyboard(&mut app, ui.ctx());
            assert!(ui.input(|i| i.events.is_empty()));
        })
        .drop_without_applying_deltas();
        assert_eq!(app.session.last_paste.as_ref().unwrap().mode, "text");
        assert_eq!(app.session.doc.plain_text(wordcraft_doc::StoryRef::Body), "rich");
        assert!(!app.paste_options.open);
        ctx.run_ui(egui::RawInput { events: vec![key(Key::Escape, true)], ..Default::default() }, |ui| keyboard(&mut app, ui.ctx()))
            .drop_without_applying_deltas();
        assert!(app.session.last_paste.is_none());
    }

    #[test]
    fn rich_paste_is_not_redirected_to_a_clipboard_picture() {
        let mut app = app();
        app.services.clipboard_rich = Some(Box::new(|| Some(json!({"text":"rich", "html":"<p><b>rich</b></p>"}))));
        app.services.clipboard_picture = Some(Box::new(|| panic!("rich text must not request a picture")));
        app.run("edit.paste", json!({})).unwrap();
        assert_eq!(app.session.doc.plain_text(wordcraft_doc::StoryRef::Body), "rich");
        assert!(app.session.last_paste.as_ref().unwrap().rich);
        app.run("edit.paste", json!({"html":"<b>explicit</b>"})).unwrap();
        assert!(app.session.doc.plain_text(wordcraft_doc::StoryRef::Body).ends_with("explicit"));
    }

    #[test]
    fn image_only_host_html_keeps_native_picture_paste() {
        let mut app = app();
        app.services.clipboard_rich = Some(Box::new(|| Some(json!({"html":"<img src='https://example.invalid/picture.png'>"}))));
        app.services.clipboard_picture =
            Some(Box::new(|| Some(crate::paste_picture::ClipboardPicture::Pixels { width: 1, height: 1, rgba: vec![0, 0, 0, 255] })));
        app.run("edit.paste", json!({})).unwrap();
        assert!(!app.session.doc.media.is_empty());
        assert!(app.session.last_paste.is_none());
        app.run("edit.undo", json!({})).unwrap();
        assert!(app.session.doc.media.is_empty());
    }

    #[test]
    fn space_chooses_without_inserting_a_space() {
        let mut app = app();
        app.run("edit.paste", json!({"text":"rich", "html":"<b>rich</b>"})).unwrap();
        app.run("ui.pasteOptions", json!({})).unwrap();
        let ctx = egui::Context::default();
        ctx.run_ui(egui::RawInput { events: vec![key(Key::Space, true), Event::Text(" ".into())], ..Default::default() }, |ui| {
            keyboard(&mut app, ui.ctx());
            assert!(ui.input(|i| i.events.is_empty()));
        })
        .drop_without_applying_deltas();
        assert_eq!(app.session.doc.plain_text(wordcraft_doc::StoryRef::Body), "rich");
    }

    fn harness(app: WordApp) -> egui_kittest::Harness<'static, WordApp> {
        let mut h = egui_kittest::Harness::builder().with_size(egui::vec2(1000.0, 700.0)).build_ui_state(
            |ui, app: &mut WordApp| {
                let ctx = ui.ctx().clone();
                app.logic(&ctx);
                app.ui(ui);
            },
            app,
        );
        for _ in 0..6 {
            h.step();
        }
        h
    }

    #[test]
    fn clicking_menu_option_reformats_without_moving_the_caret() {
        use egui_kittest::kittest::Queryable;
        let mut app = app();
        app.run("edit.paste", json!({"html":"<p><b>rich</b></p>", "text":"rich"})).unwrap();
        let mut h = harness(app);
        let button = egui::containers::AreaState::load(&h.ctx, egui::Id::new("wc_paste_options")).unwrap().rect();
        h.get_by_label("Paste Options").click();
        h.step();
        h.step();
        assert_eq!(egui::containers::AreaState::load(&h.ctx, egui::Id::new("wc_paste_options")).unwrap().rect(), button);
        let menu = egui::containers::AreaState::load(&h.ctx, egui::Id::new("wc_paste_options_menu")).unwrap().rect();
        assert!(menu.top() >= button.bottom() + 3.0);
        assert!(h.state().paste_options.rect.unwrap().contains_rect(menu));
        h.get_by_label("Keep Text Only").click();
        h.step();
        assert_eq!(h.state().session.last_paste.as_ref().unwrap().mode, "text");
        assert_eq!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body), "rich");
        h.event(Event::Text("x".into()));
        h.step();
        assert_eq!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body), "richx");
        h.state_mut().run("edit.undo", json!({})).unwrap();
        h.state_mut().run("edit.undo", json!({})).unwrap();
        assert_eq!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body), "");
    }

    #[test]
    fn dropdown_flips_above_without_moving_the_button() {
        let mut app = app();
        app.run("edit.paste", json!({"html":"<b>rich</b>", "text":"rich"})).unwrap();
        let mut h = harness(app);
        let (caret, height) = crate::canvas::caret_screen(h.state_mut()).unwrap();
        let viewport = Rect::from_min_max(Pos2::ZERO, egui::pos2(1000.0, caret.y + height + 40.0));
        let ctx = h.ctx.clone();
        ctx.run_ui(Default::default(), |ui| show(h.state_mut(), ui.ctx(), viewport)).drop_without_applying_deltas();
        let button = egui::containers::AreaState::load(&ctx, egui::Id::new("wc_paste_options")).unwrap().rect();
        h.state_mut().run("ui.pasteOptions", json!({})).unwrap();
        for _ in 0..3 {
            ctx.run_ui(Default::default(), |ui| show(h.state_mut(), ui.ctx(), viewport)).drop_without_applying_deltas();
        }
        assert_eq!(egui::containers::AreaState::load(&ctx, egui::Id::new("wc_paste_options")).unwrap().rect(), button);
        let menu = egui::containers::AreaState::load(&ctx, egui::Id::new("wc_paste_options_menu")).unwrap().rect();
        assert!(menu.bottom() <= button.top() - 3.0);
        assert!(viewport.contains_rect(menu));
    }

    #[test]
    fn typing_while_menu_is_open_dismisses_it_and_reaches_document() {
        let mut app = app();
        app.run("edit.paste", json!({"html":"<b>rich</b>", "text":"rich"})).unwrap();
        let mut h = harness(app);
        h.state_mut().run("ui.pasteOptions", json!({})).unwrap();
        h.step();
        h.step();
        h.event(Event::Text("z".into()));
        h.step();
        assert_eq!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body), "richz");
        assert!(h.state().session.last_paste.is_none());
        assert!(!h.state().paste_options.open);
    }

    #[test]
    fn scrolling_menu_offscreen_keeps_typing_in_document() {
        let mut app = app();
        app.run("edit.paste", json!({"html":"<b>rich</b>", "text":"rich"})).unwrap();
        let mut h = harness(app);
        h.state_mut().run("ui.pasteOptions", json!({})).unwrap();
        h.step();
        h.step();
        let caret = crate::canvas::caret_screen(h.state_mut()).unwrap().0;
        h.event(Event::PointerMoved(caret));
        h.step();
        h.event(Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -700.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        for _ in 0..20 {
            h.step();
        }
        assert!(!h.state().canvas.canvas_rect.unwrap().contains(crate::canvas::caret_screen(h.state_mut()).unwrap().0));
        h.event(Event::Text("z".into()));
        h.step();
        assert_eq!(h.state().session.doc.plain_text(wordcraft_doc::StoryRef::Body), "richz");
    }
}
