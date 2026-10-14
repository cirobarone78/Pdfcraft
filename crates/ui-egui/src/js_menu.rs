//! Non-modal PDF JavaScript menus. Each activation owns its receiver; retired results are discarded.

use crate::PdfCraftApp;
use pdfcraft_engine::DocId;
#[cfg(not(target_arch = "wasm32"))]
use pdfcraft_engine::js::JsOutput;
#[cfg(not(target_arch = "wasm32"))]
use pdfcraft_engine::js::{InteractiveEvent, LinkScript, MenuItem};
#[cfg(not(target_arch = "wasm32"))]
use std::sync::mpsc::{Receiver, Sender, TryRecvError};

#[cfg(not(target_arch = "wasm32"))]
const MAX_WORKERS: usize = 3;

#[cfg(not(target_arch = "wasm32"))]
#[derive(Clone, Copy, PartialEq)]
struct ViewStamp {
    page: usize,
    zoom: f32,
    rotation: u16,
    fit: crate::canvas::Fit,
    layout: crate::canvas::PageLayout,
}

#[cfg(not(target_arch = "wasm32"))]
struct Pending {
    input: LinkScript,
    anchor: egui::Pos2,
    stamp: ViewStamp,
    page: usize,
    annotation: usize,
    frame: Option<u64>,
}

#[cfg(not(target_arch = "wasm32"))]
pub(crate) struct LinkJob {
    pending: Pending,
    events: Receiver<InteractiveEvent>,
    menu: Option<(Vec<MenuItem>, Sender<Option<String>>)>,
    popup: MenuView,
    serial: u64,
    popup_number: usize,
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
pub(crate) struct LinkMenus {
    active: Option<LinkJob>,
    pending: Option<Pending>,
    retired: Vec<Receiver<InteractiveEvent>>,
    last_activation: Option<(DocId, usize, usize, u64)>,
    serial: u64,
}

#[cfg(not(target_arch = "wasm32"))]
impl LinkMenus {
    #[cfg(test)]
    fn as_ref(&self) -> Option<&LinkJob> {
        self.active.as_ref()
    }
    #[cfg(test)]
    fn is_none(&self) -> bool {
        self.active.is_none() && self.pending.is_none()
    }

    fn retire(&mut self, mut job: LinkJob, ctx: Option<&egui::Context>) {
        if let Some((_, reply)) = job.menu.take() {
            let _ = reply.send(None);
        }
        if let Some(ctx) = ctx {
            job.popup.release_focus(ctx);
        }
        self.retired.push(job.events);
    }

    fn reap(&mut self) {
        self.retired.retain(|events| {
            // The VM already caps popups at 32 per action. Never spend an unbounded frame draining.
            for _ in 0..33 {
                match events.try_recv() {
                    Ok(InteractiveEvent::Menu(_, reply)) => {
                        let _ = reply.send(None);
                    }
                    Ok(InteractiveEvent::Complete(_)) | Err(TryRecvError::Disconnected) => return false,
                    Err(TryRecvError::Empty) => return true,
                }
            }
            true
        });
    }

    pub(crate) fn inspect(&self) -> serde_json::Value {
        let (pending, popup, serial) = match &self.active {
            Some(job) => (&job.pending, Some(&job.popup), Some(job.serial)),
            None => match &self.pending {
                Some(p) => (p, None, None),
                None => return serde_json::Value::Null,
            },
        };
        serde_json::json!({ "doc": pending.input.document_id().0, "page": pending.page.saturating_add(1),
            "index": pending.annotation.saturating_add(1), "anchor": [pending.anchor.x, pending.anchor.y],
            "rects": popup.map(|p| p.rects.iter().map(|r| [r.min.x, r.min.y, r.max.x, r.max.y]).collect::<Vec<_>>()).unwrap_or_default(),
            "sequence": serial, "pending": self.pending.is_some(), "workers": self.retired.len() + usize::from(self.active.is_some()) })
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn stamp(app: &PdfCraftApp, id: DocId) -> Option<ViewStamp> {
    let view = app.views.iter().find(|v| v.id == id)?;
    Some(ViewStamp { page: view.current, zoom: view.zoom, rotation: view.rotation, fit: view.fit, layout: view.layout })
}

#[cfg(not(target_arch = "wasm32"))]
fn current(app: &PdfCraftApp, pending: &Pending) -> bool {
    app.active_ids().is_some_and(|(_, id)| id == pending.input.document_id())
        && app.session.link_javascript_is_current(&pending.input)
        && stamp(app, pending.input.document_id()) == Some(pending.stamp)
}

impl PdfCraftApp {
    /// The pointer position is captured by the canvas in viewport points, before the worker starts.
    pub(crate) fn run_link_script_at(&mut self, id: DocId, page: usize, annotation: usize, anchor: egui::Pos2, frame: Option<u64>) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            if let Some(frame) = frame {
                let key = (id, page, annotation, frame);
                if self.link_script.last_activation == Some(key) {
                    return;
                }
                self.link_script.last_activation = Some(key);
            }
            let mut state = std::mem::take(&mut self.link_script);
            if let Some(job) = state.active.take() {
                state.retire(job, None);
            }
            state.pending = None;
            match self.session.prepare_link_javascript(id, page, annotation) {
                Ok(input) => {
                    if let Some(stamp) = stamp(self, id) {
                        state.pending = Some(Pending { input, anchor, stamp, page, annotation, frame });
                    }
                }
                Err(e) => self.notify_error(e),
            }
            state.reap();
            self.start_pending_menu(&mut state);
            self.link_script = state;
        }
        #[cfg(target_arch = "wasm32")]
        {
            let _ = (id, page, annotation, anchor, frame);
            self.notify("Interactive JavaScript menus require the desktop application");
        }
    }

    #[cfg(test)]
    pub(crate) fn run_link_script(&mut self, id: DocId, page: usize, annotation: usize) {
        self.run_link_script_at(id, page, annotation, egui::pos2(160.0, 160.0), None);
    }

    #[cfg(not(target_arch = "wasm32"))]
    fn start_pending_menu(&mut self, state: &mut LinkMenus) {
        if state.active.is_some() || state.retired.len() >= MAX_WORKERS {
            return;
        }
        if let Some(pending) = state.pending.take() {
            if !current(self, &pending) {
                return;
            }
            match pending.input.start() {
                Ok(events) => {
                    // Internal UI identity, independent of document input; wrap cannot panic.
                    state.serial = state.serial.wrapping_add(1);
                    state.active = Some(LinkJob { pending, events, menu: None, popup: MenuView::default(), serial: state.serial, popup_number: 0 });
                }
                Err(e) => self.notify_error(e),
            }
        }
    }

    pub(crate) fn poll_link_script(&mut self, ctx: &egui::Context) {
        #[cfg(not(target_arch = "wasm32"))]
        {
            let mut state = std::mem::take(&mut self.link_script);
            state.reap();
            if state.pending.as_ref().is_some_and(|p| !current(self, p) || self.modal_open() || self.palette_open) {
                state.pending = None;
            }
            if let Some(mut job) = state.active.take() {
                let same_frame = job.pending.frame == Some(ctx.cumulative_frame_nr());
                let dismiss = !current(self, &job.pending)
                    || self.modal_open()
                    || self.palette_open
                    || !ctx.input(|i| i.focused)
                    || (!same_frame && job.popup.outside_interaction(ctx));
                let escape = ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
                if dismiss || escape {
                    state.retire(job, Some(ctx));
                } else {
                    let mut complete = false;
                    match job.events.try_recv() {
                        Ok(InteractiveEvent::Menu(items, reply)) => {
                            job.popup.release_focus(ctx);
                            job.popup = MenuView::default();
                            job.popup_number = job.popup_number.saturating_add(1);
                            job.menu = Some((items, reply));
                        }
                        Ok(InteractiveEvent::Complete(out)) => {
                            job.popup.release_focus(ctx);
                            match self.session.finish_link_javascript(&job.pending.input, out) {
                                Ok(out) => {
                                    let id = job.pending.input.document_id();
                                    if let Some(view) = self.views.iter_mut().find(|v| v.id == id)
                                        && let Some(doc) = self.session.get(id)
                                    {
                                        view.document_changed(&doc.info);
                                    }
                                    self.handle_js(
                                        id,
                                        JsOutput {
                                            alerts: out.alerts,
                                            console: out.console,
                                            requests: out.requests,
                                            errors: out.error.into_iter().collect(),
                                        },
                                    );
                                }
                                Err(e) => self.notify_error(e),
                            }
                            complete = true;
                        }
                        Err(TryRecvError::Disconnected) => {
                            job.popup.release_focus(ctx);
                            complete = true;
                        }
                        Err(TryRecvError::Empty) => {}
                    }
                    if !complete {
                        if let Some((items, reply)) = job.menu.take() {
                            let id = egui::Id::new(("pdf-js-menu", job.serial, job.popup_number));
                            match job.popup.show(ctx, id, job.pending.anchor, &items) {
                                Some(choice) => {
                                    let _ = reply.send(choice);
                                    job.popup.release_focus(ctx);
                                }
                                None => job.menu = Some((items, reply)),
                            }
                        }
                        state.active = Some(job);
                    }
                }
            }
            self.start_pending_menu(&mut state);
            if state.active.is_some() || state.pending.is_some() || !state.retired.is_empty() {
                ctx.request_repaint_after(std::time::Duration::from_millis(16));
            }
            self.link_script = state;
        }
        #[cfg(target_arch = "wasm32")]
        let _ = ctx;
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[derive(Default)]
struct MenuView {
    keyboard: Option<Vec<usize>>,
    opened: Vec<usize>,
    rects: Vec<egui::Rect>,
    focus_ids: Vec<egui::Id>,
    widths: std::collections::BTreeMap<Vec<usize>, f32>,
}

#[cfg(not(target_arch = "wasm32"))]
fn level<'a>(items: &'a [MenuItem], path: &[usize]) -> Option<&'a [MenuItem]> {
    let mut current = items;
    for index in path {
        current = &current.get(*index)?.children;
    }
    Some(current)
}

#[cfg(not(target_arch = "wasm32"))]
impl MenuView {
    fn width(&mut self, ctx: &egui::Context, path: &[usize], items: &[MenuItem], available: f32) -> f32 {
        let natural = *self.widths.entry(path.to_vec()).or_insert_with(|| {
            let font = egui::TextStyle::Button.resolve(&ctx.global_style());
            ctx.fonts_mut(|fonts| {
                items
                    .iter()
                    .filter(|i| i.name != "-")
                    .map(|item| fonts.layout_no_wrap(item.name.clone(), font.clone(), egui::Color32::WHITE).size().x + 32.0)
                    .fold(140.0_f32, f32::max)
                    .min(340.0)
            })
        });
        natural.min(available)
    }
    fn release_focus(&self, ctx: &egui::Context) {
        ctx.memory_mut(|m| {
            if let Some(id) = m.focused()
                && self.focus_ids.contains(&id)
            {
                m.surrender_focus(id);
            }
        });
    }

    fn outside_interaction(&self, ctx: &egui::Context) -> bool {
        ctx.input(|i| {
            let inside = i.pointer.interact_pos().is_some_and(|p| self.rects.iter().any(|r| r.contains(p)));
            // Smoothed scroll deltas can outlive a wheel event. A fresh annotation click must
            // not be cancelled by the tail of an earlier document scroll animation.
            let wheel = i.events.iter().any(|e| matches!(e, egui::Event::MouseWheel { .. }));
            ((!inside) && (i.pointer.any_pressed() || i.pointer.any_click() || wheel || i.zoom_delta() != 1.0))
                || i.events
                    .iter()
                    .any(|e| matches!(e, egui::Event::Key { pressed: true, modifiers, key, .. } if modifiers.command || *key == egui::Key::Tab))
        })
    }

    fn key(&mut self, items: &[MenuItem], key: egui::Key) -> Option<String> {
        let mut path = self.keyboard.clone().unwrap_or_default();
        let previous = path.pop();
        let siblings = level(items, &path)?;
        let enabled: Vec<_> = siblings.iter().enumerate().filter(|(_, i)| i.enabled && i.name != "-").map(|(i, _)| i).collect();
        if enabled.is_empty() {
            return None;
        }
        let selected = previous.filter(|p| enabled.contains(p)).unwrap_or_else(|| *enabled.first().unwrap_or(&0));
        let at = enabled.iter().position(|p| *p == selected).unwrap_or(0);
        let index = match key {
            egui::Key::ArrowDown if previous.is_some() => *enabled.get(at.saturating_add(1) % enabled.len()).unwrap_or(&selected),
            egui::Key::ArrowUp if previous.is_some() => {
                *enabled.get(if at == 0 { enabled.len().saturating_sub(1) } else { at - 1 }).unwrap_or(&selected)
            }
            egui::Key::ArrowUp => *enabled.last().unwrap_or(&selected),
            _ => selected,
        };
        path.push(index);
        let item = siblings.get(index)?;
        match key {
            egui::Key::ArrowRight | egui::Key::Enter | egui::Key::Space if !item.children.is_empty() => {
                if let Some(child) = item.children.iter().position(|i| i.enabled && i.name != "-") {
                    path.push(child);
                }
            }
            egui::Key::ArrowLeft if path.len() > 1 => {
                path.pop();
            }
            egui::Key::Enter | egui::Key::Space if item.children.is_empty() => return Some(item.value.clone()),
            _ => {}
        }
        self.opened = path.get(..path.len().saturating_sub(1)).unwrap_or_default().to_vec();
        self.keyboard = Some(path);
        None
    }

    /// Outer Option is a decision, inner Option is the exact JS string or cancellation.
    fn show(&mut self, ctx: &egui::Context, id: egui::Id, anchor: egui::Pos2, items: &[MenuItem]) -> Option<Option<String>> {
        let bounds = ctx.content_rect();
        if !bounds.is_finite() || bounds.width() <= 32.0 || bounds.height() <= 32.0 {
            return Some(None);
        }
        if ctx.input(|i| i.pointer.delta() != egui::Vec2::ZERO || i.smooth_scroll_delta() != egui::Vec2::ZERO) {
            self.keyboard = None;
        }
        for key in [egui::Key::ArrowDown, egui::Key::ArrowUp, egui::Key::ArrowLeft, egui::Key::ArrowRight, egui::Key::Enter, egui::Key::Space] {
            if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, key)) {
                // egui determined a spatial focus direction at begin_pass, before we consumed
                // the key. Our menu owns arrow navigation; do not move focus again at end_pass.
                ctx.memory_mut(|m| m.move_focus(egui::FocusDirection::None));
                if let Some(value) = self.key(items, key) {
                    return Some(Some(value));
                }
            }
        }
        self.rects.clear();
        self.focus_ids.clear();
        let anchor = bounds.clamp(if anchor.is_finite() { anchor } else { bounds.center() });
        let width = self.width(ctx, &[], items, bounds.width() - 32.0);
        let height = (bounds.height() - 32.0).min(420.0);
        let mut choice = None;
        if let Some(response) = menu_popup(ctx, id, anchor, width).show(|ui| {
            self.rows(ui, items, &mut Vec::new(), width, height, &mut choice);
        }) {
            self.rects.push(response.response.rect);
        }
        choice.map(Some)
    }

    fn rows(&mut self, ui: &mut egui::Ui, items: &[MenuItem], path: &mut Vec<usize>, width: f32, height: f32, choice: &mut Option<String>) {
        if path.len() > 16 {
            return;
        }
        ui.set_width(width);
        egui::ScrollArea::vertical().max_height(height).show(ui, |ui| {
            for (index, item) in items.iter().enumerate() {
                if item.name == "-" {
                    ui.separator();
                    continue;
                }
                path.push(index);
                let selected = self.keyboard.as_ref() == Some(path);
                let label = if item.marked { format!("✓ {}", item.name) } else { item.name.clone() };
                let button = egui::Button::new(label).wrap().selected(selected);
                let button = if item.children.is_empty() { button } else { button.right_text("›") };
                let response = ui.push_id(&*path, |ui| ui.add_enabled(item.enabled, button)).inner;
                self.focus_ids.push(response.id);
                if selected {
                    response.request_focus();
                    response.scroll_to_me(Some(egui::Align::Center));
                }
                if item.enabled && (response.clicked() || (self.keyboard.is_none() && response.hovered())) {
                    if response.clicked() {
                        self.keyboard = None;
                    }
                    self.opened =
                        if item.children.is_empty() { path.get(..path.len().saturating_sub(1)).unwrap_or_default().to_vec() } else { path.clone() };
                }
                if item.enabled && item.children.is_empty() && response.clicked() {
                    *choice = Some(item.value.clone());
                }
                if item.enabled && !item.children.is_empty() && self.opened.starts_with(path) {
                    let child_width = self.width(ui.ctx(), path, &item.children, (ui.ctx().content_rect().width() - 32.0).max(0.0));
                    let popup = egui::Popup::new(response.id.with("submenu"), ui.ctx().clone(), &response, ui.layer_id())
                        .kind(egui::PopupKind::Menu)
                        .align(egui::emath::RectAlign::RIGHT_START)
                        .gap(2.0)
                        .layout(egui::Layout::top_down_justified(egui::Align::Min))
                        .width(child_width)
                        .style(egui::containers::menu::menu_style)
                        .close_behavior(egui::PopupCloseBehavior::IgnoreClicks);
                    if let Some(sub) = popup.show(|ui| self.rows(ui, &item.children, path, child_width, height, choice)) {
                        self.rects.push(sub.response.rect);
                    }
                }
                path.pop();
            }
        });
    }
}

#[cfg(not(target_arch = "wasm32"))]
fn menu_popup(ctx: &egui::Context, id: egui::Id, anchor: egui::Pos2, width: f32) -> egui::Popup<'static> {
    egui::Popup::new(id, ctx.clone(), anchor, egui::LayerId::background())
        .kind(egui::PopupKind::Menu)
        .align(egui::emath::RectAlign::BOTTOM_START)
        .gap(4.0)
        .width(width)
        .layout(egui::Layout::top_down_justified(egui::Align::Min))
        .style(egui::containers::menu::menu_style)
        .close_behavior(egui::PopupCloseBehavior::IgnoreClicks)
}

#[cfg(all(test, not(target_arch = "wasm32")))]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use egui_kittest::{Harness, kittest::Queryable};
    use pdfcraft_cos::{Dict, Object, PdfString};
    fn fixture() -> Vec<u8> {
        let bytes = pdfcraft_engine::Session::new().create_blank(300.0, 300.0, 2).unwrap();
        let mut cos = pdfcraft_cos::Document::open(bytes).unwrap();
        let cat = cos.get(cos.root().unwrap());
        let pages = cos.resolve(cat.as_dict().unwrap().get(b"Pages").unwrap());
        let kids = cos.resolve(pages.as_dict().unwrap().get(b"Kids").unwrap());
        let page = kids.as_array().unwrap()[0].as_ref().unwrap();
        let mut action = Dict::new();
        action.set(b"S".to_vec(), Object::Name(b"JavaScript".to_vec()));
        action.set(b"JS".to_vec(),Object::String(PdfString::text("var c=app.popUpMenuEx({cName:'Next',cReturn:'next'},{cName:'URL',cReturn:'url'},{cName:'File',cReturn:'file'},{cName:'Unavailable',bEnabled:false}); if(c==='next')this.pageNum=1; if(c==='url')app.launchURL('https://example.org'); if(c==='file')app.openDoc('C:/pdfcraft-test/other.pdf');")));
        let action = cos.add(action);
        let mut ann = Dict::new();
        ann.set(b"Subtype".to_vec(), Object::Name(b"Link".to_vec()));
        ann.set(b"Rect".to_vec(), Object::Array(vec![Object::Int(20), Object::Int(20), Object::Int(100), Object::Int(100)]));
        ann.set(b"A".to_vec(), Object::Ref(action));
        let ann = cos.add(ann);
        let mut d = cos.get(page).as_dict().unwrap().clone();
        d.set(b"Annots".to_vec(), Object::Array(vec![Object::Ref(ann)]));
        cos.set(page, d);
        pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap()
    }
    fn harness() -> Harness<'static, PdfCraftApp> {
        let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe(|_cc| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.open_bytes("interactive.pdf", None, fixture()).unwrap();
            app
        });
        h.run_steps(4);
        wait(&mut h, |a| a.active_ids().is_some());
        h
    }
    #[track_caller]
    fn wait(h: &mut Harness<'static, PdfCraftApp>, ready: impl Fn(&PdfCraftApp) -> bool) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            h.step();
            if ready(h.state()) {
                break;
            }
            assert!(std::time::Instant::now() < deadline, "script UI timed out");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }
    #[test]
    fn link_menu_keeps_ui_responsive_and_delivers_synchronous_choice() {
        let mut h = harness();
        let id = h.state().active_ids().unwrap().1;
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, |a| a.link_script.as_ref().is_some_and(|j| j.menu.is_some()));
        h.get_by_label("Next");
        h.get_by_label("Unavailable");
        if let Ok(path) = std::env::var("PDFCRAFT_SHOT") {
            // A newly created egui Area first measures invisibly; capture its painted frame.
            h.run_steps(3);
            h.render().unwrap().save(path).unwrap();
        }
        h.get_by_label("Next").click();
        wait(&mut h, |a| a.link_script.is_none());
        assert_eq!(h.state().views[0].current, 1);
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, |a| a.link_script.as_ref().is_some_and(|j| j.menu.is_some()));
        h.get_by_label("URL").click();
        wait(&mut h, |a| a.link_script.is_none());
        assert_eq!(h.state().pending_link.as_ref().unwrap().origin, crate::LinkOrigin::Script);
        h.state_mut().pending_link = None;
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, |a| a.link_script.as_ref().is_some_and(|j| j.menu.is_some()));
        h.get_by_label("File").click();
        wait(&mut h, |a| a.link_script.is_none());
        assert!(h.state().pending_remote_link.is_some());
        assert_eq!(h.state().views.len(), 1, "no file is read/opened before consent");
    }
    #[test]
    fn disabling_javascript_cancels_a_waiting_menu() {
        let mut h = harness();
        let id = h.state().active_ids().unwrap().1;
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, |a| a.link_script.as_ref().is_some_and(|j| j.menu.is_some()));
        h.state_mut().session.set_javascript(false);
        h.step();
        assert!(h.state().link_script.is_none());
        assert_eq!(h.state().views[0].current, 0);
        h.state_mut().run_link_script(id, 0, 0);
        h.step();
        assert!(h.state().link_script.is_none());
    }

    fn second_link(bytes: Vec<u8>) -> Vec<u8> {
        let mut cos = pdfcraft_cos::Document::open(std::sync::Arc::new(bytes)).unwrap();
        let cat = cos.get(cos.root().unwrap());
        let pages = cos.resolve(cat.as_dict().unwrap().get(b"Pages").unwrap());
        let kids = cos.resolve(pages.as_dict().unwrap().get(b"Kids").unwrap());
        let page = kids.as_array().unwrap()[0].as_ref().unwrap();
        let mut action = Dict::new();
        action.set(b"S".to_vec(), Object::Name(b"JavaScript".to_vec()));
        action.set(
            b"JS".to_vec(),
            Object::String(PdfString::text("console.println('started B'); app.popUpMenuEx({cName:'Second component',cReturn:'B'});")),
        );
        let action = cos.add(action);
        let mut ann = Dict::new();
        ann.set(b"Subtype".to_vec(), Object::Name(b"Link".to_vec()));
        ann.set(b"Rect".to_vec(), Object::Array(vec![Object::Int(200), Object::Int(200), Object::Int(260), Object::Int(260)]));
        ann.set(b"A".to_vec(), Object::Ref(action));
        let ann = cos.add(ann);
        let mut d = cos.get(page).as_dict().unwrap().clone();
        let mut annotations = d.get(b"Annots").unwrap().as_array().unwrap().clone();
        annotations.push(Object::Ref(ann));
        d.set(b"Annots".to_vec(), Object::Array(annotations));
        cos.set(page, d);
        pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap()
    }

    #[test]
    fn a_second_activation_replaces_the_waiting_menu() {
        let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe(|_cc| {
            let mut app = PdfCraftApp::new();
            app.set_option("language", "en").unwrap();
            app.open_bytes("two-links.pdf", None, second_link(fixture())).unwrap();
            app
        });
        wait(&mut h, |a| a.active_ids().is_some());
        let id = h.state().active_ids().unwrap().1;
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, |a| a.link_script.as_ref().is_some_and(|j| j.menu.is_some()));
        h.state_mut().run_link_script(id, 0, 1);
        h.run_steps(3);
        let menu = &h.state().link_script.as_ref().unwrap().menu.as_ref().unwrap().0;
        assert_eq!(menu[0].name, "Second component", "the second activation must replace the first, without Escape");
    }
    fn ready_menu(a: &PdfCraftApp) -> bool {
        a.link_script.as_ref().is_some_and(|j| j.menu.is_some() && !j.popup.rects.is_empty())
    }
    fn click_at(h: &mut Harness<'static, PdfCraftApp>, point: egui::Pos2) {
        h.hover_at(point);
        h.step();
        h.drag_at(point);
        h.step();
        h.drop_at(point);
        h.step();
    }
    fn two_links(ppp: f32, zoom: &str) -> Harness<'static, PdfCraftApp> {
        let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).with_pixels_per_point(ppp).build_eframe(|_cc| {
            let mut a = PdfCraftApp::new();
            a.set_option("language", "en").unwrap();
            a.open_bytes("two-links.pdf", None, second_link(fixture())).unwrap();
            a
        });
        wait(&mut h, |a| a.active_ids().is_some());
        h.state_mut().set_option("mode", "read").unwrap();
        h.state_mut().set_option("zoom", zoom).unwrap();
        h.run_steps(4);
        h
    }
    fn at(h: &Harness<'static, PdfCraftApp>, x: f32, y: f32) -> egui::Pos2 {
        let r = h.state().views[0].page_screen_rect(0).unwrap();
        egui::pos2(r.min.x + r.width() * x / 300.0, r.max.y - r.height() * y / 300.0)
    }
    #[test]
    fn canvas_single_click_replaces_a_menu_at_zoom_and_dpi_without_replay() {
        for (ppp, zoom) in [(1.0, "50%"), (1.25, "100%"), (2.0, "200%")] {
            let mut h = two_links(ppp, zoom);
            let a = at(&h, 60.0, 60.0);
            click_at(&mut h, a);
            wait(&mut h, ready_menu);
            let first = h.state().link_script.as_ref().unwrap().serial;
            assert!((h.state().link_script.as_ref().unwrap().pending.anchor - a).length() < 1.0);
            let b = at(&h, 230.0, 230.0);
            click_at(&mut h, b);
            wait(&mut h, |a| {
                a.link_script.as_ref().is_some_and(|j| j.menu.as_ref().is_some_and(|m| m.0.first().is_some_and(|i| i.name == "Second component")))
            });
            let job = h.state().link_script.as_ref().unwrap();
            assert_eq!(job.serial, first + 1);
            assert!((job.pending.anchor - b).length() < 1.0);
            h.get_by_label("Second component").click();
            wait(&mut h, |a| a.link_script.is_none());
            assert_eq!(h.state().js_console.log.iter().filter(|s| s.as_str() == "started B").count(), 1);
        }
    }
    #[test]
    fn popup_flips_at_edges_stays_in_viewport_and_cancels_without_eating_the_click() {
        let mut h = harness();
        let id = h.state().active_ids().unwrap().1;
        for anchor in [egui::pos2(20.0, 20.0), egui::pos2(1190.0, 790.0)] {
            h.state_mut().run_link_script_at(id, 0, 0, anchor, None);
            wait(&mut h, ready_menu);
            h.run_steps(2);
            let r = h.state().link_script.as_ref().unwrap().popup.rects.last().copied().unwrap();
            assert!(r.min.x >= -1.0 && r.min.y >= -1.0 && r.max.x <= 1201.0 && r.max.y <= 801.0, "{r:?}");
            if anchor.x > 1000.0 {
                assert!(r.min.x < anchor.x && r.min.y < anchor.y);
            } else {
                assert!(r.min.x >= anchor.x && r.min.y >= anchor.y);
            }
            h.key_press(egui::Key::Escape);
            h.step();
            assert!(h.state().link_script.is_none());
        }
        h.state_mut().run_link_script_at(id, 0, 0, egui::pos2(500.0, 400.0), None);
        wait(&mut h, ready_menu);
        click_at(&mut h, egui::pos2(1120.0, 740.0));
        assert!(h.state().link_script.is_none());
        assert!(!h.state().session.get(id).unwrap().dirty);
    }
    #[test]
    fn replacement_sends_null_and_discards_late_results() {
        let mut h = harness();
        let id = h.state().active_ids().unwrap().1;
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, ready_menu);
        let (tx, rx) = std::sync::mpsc::channel();
        h.state_mut().link_script.active.as_mut().unwrap().menu.as_mut().unwrap().1 = tx;
        // Replace the event source with a delayed host to exercise stale completion deterministically.
        let (late_tx, late_rx) = std::sync::mpsc::channel();
        h.state_mut().link_script.active.as_mut().unwrap().events = late_rx;
        h.state_mut().run_link_script(id, 0, 0);
        assert_eq!(rx.recv_timeout(std::time::Duration::from_secs(1)).unwrap(), None);
        late_tx
            .send(InteractiveEvent::Complete(pdfcraft_engine::js::Outcome {
                requests: vec![pdfcraft_engine::js::Request::LaunchUrl("https://obsolete.example".into())],
                ..Default::default()
            }))
            .unwrap();
        h.step();
        assert!(h.state().pending_link.is_none());
        wait(&mut h, ready_menu);
        assert!(h.state().link_script.active.is_some());
    }
    #[test]
    fn rapid_activations_are_bounded_and_latest_wins_then_tab_change_cancels() {
        let mut h = two_links(1.0, "100%");
        let id = h.state().active_ids().unwrap().1;
        for i in 0..25 {
            h.state_mut().run_link_script(id, 0, i % 2);
            let state = &h.state().link_script;
            assert!(state.retired.len() + usize::from(state.active.is_some()) <= MAX_WORKERS);
        }
        h.state_mut().run_link_script(id, 0, 1);
        wait(&mut h, |a| {
            a.link_script.active.as_ref().is_some_and(|j| j.menu.as_ref().is_some_and(|m| m.0.first().is_some_and(|i| i.name == "Second component")))
        });
        let bytes = fixture();
        h.state_mut().open_bytes("other.pdf", None, bytes).unwrap();
        wait(&mut h, |a| a.views.len() == 2);
        h.step();
        assert!(h.state().link_script.is_none());
        h.state_mut().active = Some(0);
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, ready_menu);
        h.state_mut().close_tab(0);
        h.step();
        assert!(h.state().link_script.is_none());
    }
    fn entry(name: &str, enabled: bool, children: Vec<MenuItem>) -> MenuItem {
        MenuItem { name: name.into(), value: format!("return {name}"), enabled, marked: false, children }
    }
    #[test]
    fn keyboard_skips_disabled_and_separators_and_enters_and_leaves_submenus() {
        let items = vec![
            entry("disabled", false, vec![]),
            entry("-", true, vec![]),
            entry("submenu", true, vec![entry("disabled child", false, vec![]), entry("child", true, vec![])]),
            entry("last", true, vec![]),
        ];
        let mut view = MenuView::default();
        view.key(&items, egui::Key::ArrowDown);
        assert_eq!(view.keyboard, Some(vec![2]));
        view.key(&items, egui::Key::ArrowRight);
        assert_eq!(view.keyboard, Some(vec![2, 1]));
        view.key(&items, egui::Key::ArrowLeft);
        assert_eq!(view.keyboard, Some(vec![2]));
        assert!(view.opened.is_empty());
        view.key(&items, egui::Key::Enter);
        assert_eq!(view.keyboard, Some(vec![2, 1]));
        assert_eq!(view.key(&items, egui::Key::Enter).as_deref(), Some("return child"));
    }

    fn scripted_harness(script: &str) -> Harness<'static, PdfCraftApp> {
        let mut cos = pdfcraft_cos::Document::open(std::sync::Arc::new(fixture())).unwrap();
        let cat = cos.get(cos.root().unwrap());
        let pages = cos.resolve(cat.as_dict().unwrap().get(b"Pages").unwrap());
        let kids = cos.resolve(pages.as_dict().unwrap().get(b"Kids").unwrap());
        let page = cos.get(kids.as_array().unwrap()[0].as_ref().unwrap());
        let annots = cos.resolve(page.as_dict().unwrap().get(b"Annots").unwrap());
        let annot = cos.resolve(&annots.as_array().unwrap()[0]);
        let action = annot.as_dict().unwrap().get(b"A").unwrap().as_ref().unwrap();
        let mut d = cos.get(action).as_dict().unwrap().clone();
        d.set(b"JS".to_vec(), Object::String(PdfString::text(script)));
        cos.set(action, d);
        let bytes = pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap();
        let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe(|_cc| {
            let mut a = PdfCraftApp::new();
            a.set_option("language", "en").unwrap();
            a.open_bytes("scripted.pdf", None, bytes.clone()).unwrap();
            a
        });
        wait(&mut h, |a| a.active_ids().is_some());
        h.run_steps(3);
        let id = h.state().active_ids().unwrap().1;
        h.state_mut().run_link_script_at(id, 0, 0, egui::pos2(350.0, 220.0), None);
        wait(&mut h, ready_menu);
        h
    }

    #[test]
    fn keyboard_focus_matches_selection_and_submenu_returns_the_default_name() {
        let mut h = scripted_harness(
            "var c=app.popUpMenuEx({cName:'Group',oSubMenu:[{cName:'Disabled child',bEnabled:false},{cName:'Marked child',bMarked:true,cReturn:'next'},{cName:'Label only'}]});console.println('result '+c);if(c==='next')this.pageNum=1;",
        );
        h.key_press(egui::Key::ArrowDown);
        h.step();
        let root_id = h.state().link_script.active.as_ref().unwrap().popup.focus_ids[0];
        assert_eq!(h.ctx.memory(|m| m.focused()), Some(root_id));
        h.key_press(egui::Key::ArrowRight);
        h.step();
        h.run_steps(2);
        assert_eq!(h.state().link_script.active.as_ref().unwrap().popup.keyboard, Some(vec![0, 1]));
        h.get_by_label("Disabled child");
        h.get_by_label("✓ Marked child");
        h.key_press(egui::Key::ArrowDown);
        h.step();
        assert_eq!(h.state().link_script.active.as_ref().unwrap().popup.keyboard, Some(vec![0, 2]));
        h.key_press(egui::Key::Enter);
        wait(&mut h, |a| a.link_script.is_none());
        assert!(h.state().js_console.log.iter().any(|s| s == "result Label only"));
        assert_eq!(h.state().views[0].current, 0);
    }

    #[test]
    fn long_wrapped_lists_scroll_within_the_menu_and_pointer_submenus_select() {
        let mut h = scripted_harness(
            "var a=[];for(var i=0;i<60;i++)a.push({cName:'Long property description with enough text to wrap to multiple lines '+i});app.popUpMenuEx.apply(app,a);",
        );
        h.run_steps(2);
        let rect = *h.state().link_script.active.as_ref().unwrap().popup.rects.last().unwrap();
        assert!(rect.width() <= 372.0 && rect.height() <= 452.0, "{rect:?}");
        h.hover_at(rect.center());
        h.step();
        h.event(egui::Event::MouseWheel {
            unit: egui::MouseWheelUnit::Point,
            delta: egui::vec2(0.0, -300.0),
            modifiers: egui::Modifiers::NONE,
            phase: egui::TouchPhase::Move,
        });
        h.step();
        assert!(ready_menu(h.state()));
        assert_eq!(h.state().views[0].current, 0);
        h.key_press(egui::Key::Escape);
        h.step();
        assert!(h.state().link_script.is_none());
        let mut h = scripted_harness(
            "var c=app.popUpMenuEx({cName:'Group',oSubMenu:[{cName:'Disabled child',bEnabled:false},{cName:'Child',cReturn:'selected'}]});console.println(c);",
        );
        let point = h.get_by_label("Group ›").rect().center();
        h.hover_at(point);
        h.run_steps(3);
        h.get_by_label("Disabled child");
        h.get_by_label("Child").click();
        wait(&mut h, |a| a.link_script.is_none());
        assert!(h.state().js_console.log.iter().any(|s| s == "selected"));
    }
    #[test]
    fn ordinary_goto_link_after_popup_is_not_lost() {
        let mut bytes = second_link(fixture());
        let mut cos = pdfcraft_cos::Document::open(std::sync::Arc::new(bytes)).unwrap();
        let cat = cos.get(cos.root().unwrap());
        let pages = cos.resolve(cat.as_dict().unwrap().get(b"Pages").unwrap());
        let kids = cos.resolve(pages.as_dict().unwrap().get(b"Kids").unwrap());
        let page = kids.as_array().unwrap()[0].as_ref().unwrap();
        let target = kids.as_array().unwrap()[1].as_ref().unwrap();
        let mut annotation = Dict::new();
        annotation.set(b"Subtype".to_vec(), Object::Name(b"Link".to_vec()));
        annotation.set(b"Rect".to_vec(), Object::Array(vec![Object::Int(200), Object::Int(120), Object::Int(260), Object::Int(170)]));
        annotation.set(b"Dest".to_vec(), Object::Array(vec![Object::Ref(target), Object::Name(b"Fit".to_vec())]));
        let annotation = cos.add(annotation);
        let mut page_dict = cos.get(page).as_dict().unwrap().clone();
        let mut a = page_dict.get(b"Annots").unwrap().as_array().unwrap().clone();
        a.push(Object::Ref(annotation));
        page_dict.set(b"Annots".to_vec(), Object::Array(a));
        cos.set(page, page_dict);
        bytes = pdfcraft_cos::write_incremental(&cos, &Default::default()).unwrap();
        let mut h = Harness::builder().with_size(egui::vec2(1200.0, 800.0)).build_eframe(|_cc| {
            let mut a = PdfCraftApp::new();
            a.open_bytes("goto.pdf", None, bytes.clone()).unwrap();
            a
        });
        wait(&mut h, |a| a.active_ids().is_some());
        h.run_steps(4);
        let id = h.state().active_ids().unwrap().1;
        h.state_mut().run_link_script_at(id, 0, 0, egui::pos2(150.0, 160.0), None);
        wait(&mut h, ready_menu);
        let point = at(&h, 230.0, 145.0);
        click_at(&mut h, point);
        h.run_steps(4);
        assert_eq!(h.state().views[0].current, 1);
        assert!(h.state().link_script.is_none());
    }

    #[test]
    fn themes_and_outside_scroll_zoom_and_focus_loss_keep_document_interactions_available() {
        let mut h = harness();
        let id = h.state().active_ids().unwrap().1;
        for theme in ["light", "dark"] {
            h.state_mut().set_option("theme", theme).unwrap();
            h.state_mut().run_link_script_at(id, 0, 0, egui::pos2(500.0, 400.0), None);
            wait(&mut h, ready_menu);
            h.run_steps(3);
            h.get_by_label("Next");
            if let Ok(directory) = std::env::var("PDFCRAFT_MENU_SHOTS") {
                h.render().unwrap().save(std::path::Path::new(&directory).join(format!("menu-{theme}.png"))).unwrap();
            }
            h.hover_at(egui::pos2(1000.0, 650.0));
            h.event(egui::Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, -30.0),
                modifiers: egui::Modifiers::NONE,
                phase: egui::TouchPhase::Move,
            });
            h.step();
            assert!(h.state().link_script.is_none());
        }
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, ready_menu);
        h.state_mut().set_option("zoom", "200%").unwrap();
        h.step();
        assert!(h.state().link_script.is_none());
        h.state_mut().run_link_script(id, 0, 0);
        wait(&mut h, ready_menu);
        h.input_mut().focused = false;
        h.event(egui::Event::WindowFocused(false));
        h.step();
        assert!(h.state().link_script.is_none());
        assert!(!h.state().session.get(id).unwrap().dirty);
    }
}
