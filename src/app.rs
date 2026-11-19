use crate::{
    carousel::{Carousel, WheelUnit},
    core::{Settings, Store, TrayAppearance, same_path},
    gallery::{Direction, Grid, Transition},
    platform::{self, Composition, GalleryBounds, Platform, PlatformEvent},
    thumbnails::{ThumbState, Thumbnails},
    worker::{self, Command, Worker},
};
use eframe::egui::{
    self, Align, Align2, Color32, FontId, Key, Layout, Pos2, Rect, RichText, Sense, Stroke,
    StrokeKind, Vec2,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use std::{
    path::{Path, PathBuf},
    sync::mpsc::{self, Receiver},
    time::{Duration, Instant},
};

const INK: Color32 = Color32::from_rgb(239, 239, 233);
const MUTED: Color32 = Color32::from_rgb(183, 188, 187);
const ACCENT: Color32 = Color32::from_rgb(194, 217, 184);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OverlayState {
    Hidden,
    Browsing,
    Preferences,
    FilePicker,
}
impl OverlayState {
    fn visible(self) -> bool {
        matches!(self, Self::Browsing | Self::Preferences)
    }
}

// Animate only the client content. DWM keeps the Acrylic surface, clipping and
// hit testing throughout. Retarget from the sampled value, even mid-dismissal.
struct Reveal {
    since: Instant,
    from: f32,
    opening: bool,
}

impl Reveal {
    fn hidden(now: Instant) -> Self {
        Self {
            since: now,
            from: 0.0,
            opening: false,
        }
    }

    fn value(&self, now: Instant) -> f32 {
        let end = if self.opening { 1.0 } else { 0.0 };
        let seconds = if self.opening { 0.110 } else { 0.065 };
        let distance = (end - self.from).abs();
        if distance == 0.0 {
            return end;
        }
        let t = (now.saturating_duration_since(self.since).as_secs_f32() / (seconds * distance))
            .min(1.0);
        end + (self.from - end) * (1.0 - t).powi(3)
    }

    fn target(&mut self, opening: bool, now: Instant, reduced: bool) {
        if self.opening != opening || reduced {
            self.from = if reduced {
                if opening { 1.0 } else { 0.0 }
            } else {
                self.value(now)
            };
            self.opening = opening;
            self.since = now;
        }
    }
}

pub struct WallApp {
    ctx: egui::Context,
    hwnd: isize,
    platform: Platform,
    platform_events: Receiver<PlatformEvent>,
    worker: Worker,
    thumbnails: Thumbnails,
    paths: Vec<PathBuf>,
    settings: Settings,
    draft: Settings,
    wordmark: Option<egui::TextureHandle>,
    startup: Option<bool>,
    draft_startup: bool,
    carousel: Carousel,
    gallery: Transition,
    gallery_bounds: Option<GalleryBounds>,
    gallery_scroll: f32,
    current: Option<PathBuf>,
    state: OverlayState,
    first: bool,
    background: bool,
    quitting: bool,
    composition: Composition,
    reduced_motion: bool,
    reveal: Reveal,
    pending_edits: u32,
    applying: bool,
    #[cfg(debug_assertions)]
    apply_started: Option<Instant>,
    last_frame: Instant,
    interaction: u64,
    query: u64,
    center_on_reply: Option<(u64, u64)>,
    notice: Option<(String, Instant)>,
    imported_start: Option<usize>,
}

impl WallApp {
    pub fn new(
        cc: &eframe::CreationContext<'_>,
        store: Store,
        background: bool,
        validate_ui: bool,
    ) -> Result<Self, String> {
        let hwnd = match cc.window_handle().map_err(|e| e.to_string())?.as_raw() {
            RawWindowHandle::Win32(handle) => handle.hwnd.get(),
            _ => return Err("wall requires Windows 11".into()),
        };
        style(&cc.egui_ctx);
        let (sender, platform_events) = mpsc::channel();
        let settings = store.collection.settings.clone();
        let paths = store.collection.paths.clone();
        let thumbnails = Thumbnails::new(cc.egui_ctx.clone(), store.thumbnail_dir());
        let platform = Platform::start(
            cc.egui_ctx.clone(),
            sender,
            settings.hotkey,
            settings.tray_appearance,
        )?;
        let worker = Worker::new(store, cc.egui_ctx.clone(), validate_ui)?;
        let composition = platform::configure_window(hwnd, settings.glass);
        platform.set_overlay(hwnd, false)?;
        cc.egui_ctx.request_repaint();
        Ok(Self {
            ctx: cc.egui_ctx.clone(),
            hwnd,
            platform,
            platform_events,
            worker,
            thumbnails,
            paths,
            draft: settings.clone(),
            wordmark: None,
            startup: None,
            draft_startup: false,
            settings,
            carousel: Carousel::new(0),
            gallery: Transition::default(),
            gallery_bounds: None,
            gallery_scroll: 0.0,
            current: None,
            state: OverlayState::Hidden,
            first: true,
            background,
            quitting: false,
            composition,
            reduced_motion: platform::reduced_motion(),
            reveal: Reveal::hidden(Instant::now()),
            pending_edits: 0,
            applying: false,
            #[cfg(debug_assertions)]
            apply_started: None,
            last_frame: Instant::now(),
            interaction: 0,
            query: 0,
            center_on_reply: None,
            notice: None,
            imported_start: None,
        })
    }

    fn tell(&mut self, message: impl Into<String>) {
        self.notice = Some((message.into(), Instant::now()));
        if self.state.visible() {
            self.ctx.request_repaint_after(Duration::from_secs(7));
        }
    }

    fn send(&mut self, command: Command) {
        if let Err(error) = self.worker.send(command) {
            self.pending_edits = 0;
            self.applying = false;
            if self.state == OverlayState::FilePicker {
                self.present(&self.ctx.clone(), false);
            }
            self.tell(error);
        }
    }

    fn show(&mut self, ctx: &egui::Context) {
        self.present(ctx, true);
    }

    fn present(&mut self, ctx: &egui::Context, center_current: bool) {
        self.present_at(ctx, center_current, None);
    }

    fn present_at(
        &mut self,
        ctx: &egui::Context,
        center_current: bool,
        cursor: Option<(i32, i32)>,
    ) {
        self.state = OverlayState::Browsing;
        let follow_desktop = center_current && !self.gallery.expanded();
        if center_current {
            self.gallery = Transition::default();
            self.gallery_scroll = 0.0;
            self.thumbnails.set_gallery(false);
        }
        if follow_desktop {
            let selected = self
                .current
                .as_ref()
                .and_then(|p| self.paths.iter().position(|v| same_path(v, p)))
                .unwrap_or(self.carousel.target.min(self.paths.len().saturating_sub(1)));
            self.carousel.snap(selected);
        }
        self.last_frame = Instant::now();
        self.reduced_motion = platform::reduced_motion();
        self.reveal
            .target(true, self.last_frame, self.reduced_motion);
        self.thumbnails.invalidate();
        self.preload_near_selection();
        if let Err(error) = self.platform.set_overlay(self.hwnd, true) {
            self.tell(error);
        }
        platform::show_overlay(self.hwnd, cursor);
        self.gallery_bounds = GalleryBounds::capture(self.hwnd, self.paths.len()).ok();
        if self.gallery.expanded() {
            self.thumbnails.set_gallery(true);
            self.gallery.target(true, true);
            self.resize_gallery();
        }
        self.query = self.query.wrapping_add(1);
        self.center_on_reply = follow_desktop.then_some((self.query, self.interaction));
        self.send(Command::Current(self.query));
        ctx.request_repaint();
    }

    fn hide(&mut self, ctx: &egui::Context) {
        if self.reduced_motion {
            self.hide_now(ctx);
        } else {
            self.reveal.target(false, Instant::now(), false);
            ctx.request_repaint();
        }
    }

    fn hide_now(&mut self, ctx: &egui::Context) {
        self.reveal = Reveal::hidden(Instant::now());
        self.state = OverlayState::Hidden;
        self.center_on_reply = None;
        self.carousel.snap(self.carousel.target);
        self.thumbnails.suspend();
        self.thumbnails.set_gallery(false);
        let _ = self.platform.set_overlay(self.hwnd, false);
        platform::hide_overlay(self.hwnd);
        #[cfg(debug_assertions)]
        if let Some(start) = self.apply_started.take() {
            eprintln!("[apply] accepted to HWND hidden: {:?}", start.elapsed());
        }
        ctx.request_repaint();
    }

    fn pick(&mut self) {
        if self.state == OverlayState::FilePicker || self.applying {
            return;
        }
        self.hide_now(&self.ctx.clone());
        self.state = OverlayState::FilePicker;
        self.pending_edits += 1;
        self.send(Command::PickImages(worker::ImportContext {
            epoch: self.interaction,
            reveal: true,
        }));
    }

    fn apply(&mut self) {
        let input = Instant::now();
        if self.applying {
            return;
        }
        if let Some(path) = self.paths.get(self.carousel.target).cloned() {
            match self.worker.send(Command::Apply(path, input)) {
                Ok(()) => {
                    #[cfg(debug_assertions)]
                    {
                        self.apply_started = Some(input);
                    }
                    self.applying = true;
                    self.notice = None;
                    self.hide(&self.ctx.clone());
                }
                Err(error) => self.tell(error),
            }
        }
    }

    fn remove(&mut self, path: PathBuf) {
        if !self.applying {
            self.pending_edits += 1;
            self.send(Command::Remove(path));
        }
    }

    fn select(&mut self, index: usize) {
        if self.carousel.select(index, self.paths.len()) {
            self.reveal_gallery_selection();
            self.interacted();
        }
    }

    fn expanded_grid(&self, center: Pos2) -> Option<Grid> {
        let (_, open) = self
            .gallery_bounds
            .as_ref()?
            .sizes(self.ctx.pixels_per_point());
        let outer = Rect::from_center_size(center, open);
        let area = Rect::from_min_max(
            outer.min + Vec2::new(28.0, 36.0),
            outer.max - Vec2::new(28.0, 48.0),
        );
        Some(Grid::new(area, self.paths.len()))
    }

    fn reveal_gallery_selection(&mut self) {
        if self.gallery.expanded()
            && let Some(grid) = self.expanded_grid(Pos2::ZERO)
        {
            self.scroll_gallery(
                &grid,
                grid.reveal(self.carousel.target, self.gallery_scroll),
            );
        }
    }

    fn scroll_gallery(&mut self, grid: &Grid, scroll: f32) {
        if self.gallery_scroll != scroll {
            self.gallery_scroll = scroll;
            self.thumbnails
                .retain_queued(&self.paths[grid.visible(scroll)]);
            self.interacted();
        }
    }

    fn set_gallery(&mut self, expanded: bool) {
        if self.paths.is_empty() && expanded {
            return;
        }
        if self.gallery_bounds.is_none() {
            self.tell("Gallery cannot resize on this monitor");
            return;
        }
        let at_rest = !self.gallery.active();
        self.gallery.target(expanded, self.reduced_motion);
        if expanded {
            self.thumbnails.set_gallery(true);
            if at_rest {
                self.reveal_gallery_selection();
            }
        } else if at_rest {
            self.carousel.snap(self.carousel.target);
        }
        self.interacted();
    }

    fn resize_gallery(&mut self) {
        if let Some(bounds) = &mut self.gallery_bounds
            && let Err(error) = bounds.resize(self.hwnd, self.gallery.progress())
        {
            self.gallery = Transition::default();
            self.gallery_bounds = None;
            self.thumbnails.set_gallery(false);
            self.tell(error);
        }
    }

    fn interacted(&mut self) {
        self.interaction = self.interaction.wrapping_add(1);
        self.center_on_reply = None;
        self.ctx.request_repaint();
    }

    fn preload_near_selection(&mut self) {
        let len = self.paths.len();
        let target = self.carousel.target;
        // Queue far-to-near: the decoder's LIFO prioritizes the central image.
        for distance in (0..=2).rev() {
            for index in [
                target.checked_sub(distance),
                target.checked_add(distance).filter(|i| *i < len),
            ]
            .into_iter()
            .flatten()
            {
                if let Some(path) = self.paths.get(index) {
                    let _ = self.thumbnails.get(path);
                }
            }
        }
    }

    fn events(&mut self, ctx: &egui::Context) {
        while let Ok(event) = self.platform_events.try_recv() {
            if self.state == OverlayState::FilePicker
                && !matches!(
                    event,
                    PlatformEvent::Quit | PlatformEvent::Error(_) | PlatformEvent::MotionChanged(_)
                )
            {
                continue;
            }
            match event {
                PlatformEvent::Toggle(cursor) => {
                    if self.state.visible() && self.reveal.opening {
                        self.hide(ctx);
                    } else {
                        self.present_at(ctx, true, cursor);
                    }
                }
                PlatformEvent::Show => self.show(ctx),
                PlatformEvent::Import => self.pick(),
                PlatformEvent::Settings => {
                    if !self.state.visible() || !self.reveal.opening {
                        self.show(ctx);
                    }
                    self.open_preferences();
                }
                PlatformEvent::OutsideClick { explorer } => {
                    // Focus changes never dismiss. Explorer and the taskbar remain
                    // usable while this window is an OLE drop target.
                    if !explorer && self.state.visible() {
                        self.hide(ctx);
                    }
                }
                PlatformEvent::MotionChanged(reduced) => {
                    self.reduced_motion = reduced;
                    self.reveal
                        .target(self.reveal.opening, Instant::now(), reduced);
                    self.composition = platform::configure_window(self.hwnd, self.settings.glass);
                }
                PlatformEvent::Quit => {
                    self.quitting = true;
                    ctx.send_viewport_cmd(egui::ViewportCommand::Close);
                }
                PlatformEvent::Error(error) => {
                    self.tell(error);
                    if self.state == OverlayState::Hidden {
                        self.show(ctx);
                    }
                }
            }
        }
        while let Ok(event) = self.worker.events.try_recv() {
            match event {
                worker::Event::Startup(result) => match result {
                    Ok(enabled) => {
                        self.startup = Some(enabled);
                        self.draft_startup = enabled;
                    }
                    Err(error) => self.tell(error),
                },
                #[cfg(debug_assertions)]
                worker::Event::Diagnostic(result) => match result {
                    Ok(_) => self.tell("Diagnostic snapshot saved"),
                    Err(error) => self.tell(error),
                },
                worker::Event::Snapshot {
                    collection,
                    message,
                    reopen,
                    imported,
                } => {
                    self.pending_edits = self.pending_edits.saturating_sub(1);
                    let old_index = self.carousel.target;
                    let previous = self.paths.get(old_index).cloned();
                    let old_len = self.paths.len();
                    self.paths = collection.paths;
                    let new_index = previous
                        .and_then(|p| self.paths.iter().position(|v| same_path(v, &p)))
                        .unwrap_or(old_index.min(self.paths.len().saturating_sub(1)));
                    self.carousel.rebase_after_collection_change(
                        old_index,
                        new_index,
                        self.paths.len(),
                    );
                    if let Some((index, context)) = imported
                        && context.reveal
                        && context.epoch == self.interaction
                        && self.carousel.is_settled()
                    {
                        self.select(index);
                    }
                    if self.paths.is_empty() && self.gallery.expanded() {
                        self.set_gallery(false);
                    } else if self.carousel.target != old_index {
                        self.reveal_gallery_selection();
                    }
                    if self.paths.len() > old_len {
                        self.imported_start = Some(old_len);
                    }
                    if let Err(error) = self.platform.set_hotkey(collection.settings.hotkey) {
                        self.tell(error);
                    }
                    let tray_changed =
                        self.settings.tray_appearance != collection.settings.tray_appearance;
                    if tray_changed
                        && let Err(error) = self
                            .platform
                            .set_tray_appearance(collection.settings.tray_appearance)
                    {
                        self.tell(error);
                    }
                    let changed = self.settings.glass != collection.settings.glass
                        || self.settings.hotkey != collection.settings.hotkey
                        || tray_changed;
                    if self.state != OverlayState::Preferences || changed {
                        self.draft = collection.settings.clone();
                    }
                    if self.settings.glass != collection.settings.glass {
                        self.composition =
                            platform::configure_window(self.hwnd, collection.settings.glass);
                    }
                    self.settings = collection.settings;
                    if let Some(message) = message {
                        self.tell(message);
                    }
                    if reopen {
                        self.present(ctx, false);
                    }
                    if self.state == OverlayState::Browsing {
                        self.preload_near_selection();
                    }
                }
                worker::Event::Applied(result) => {
                    self.applying = false;
                    match result {
                        Ok(path) => {
                            self.current = Some(path);
                        }
                        Err(error) => {
                            if !self.state.visible() {
                                self.present(ctx, false);
                            } else {
                                self.reveal
                                    .target(true, Instant::now(), self.reduced_motion);
                            }
                            self.tell(error);
                        }
                    }
                }
                worker::Event::Current { request, result } => {
                    if request != self.query {
                        continue;
                    }
                    match result {
                        Ok(current) => {
                            self.current = current;
                            if self.center_on_reply == Some((request, self.interaction))
                                && let Some(index) = self
                                    .current
                                    .as_ref()
                                    .and_then(|p| self.paths.iter().position(|v| same_path(v, p)))
                            {
                                self.carousel.snap(index);
                                self.preload_near_selection();
                            }
                            self.center_on_reply = None;
                        }
                        Err(error) => self.tell(error),
                    }
                }
            }
        }
    }

    fn keyboard(&mut self, ctx: &egui::Context) {
        #[cfg(debug_assertions)]
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F10)) {
            self.send(Command::Capture(self.hwnd));
        }
        if egui::Popup::is_any_open(ctx) {
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Escape)) {
            if self.state == OverlayState::Preferences {
                self.state = OverlayState::Browsing;
            } else {
                self.hide(ctx);
            }
            return;
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::Q)) {
            self.quitting = true;
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::O)) {
            self.pick();
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::CTRL, Key::Comma)) {
            if self.state == OverlayState::Preferences {
                self.state = OverlayState::Browsing;
            } else {
                self.open_preferences();
            }
        }
        if self.state != OverlayState::Browsing || self.applying || ctx.text_edit_focused() {
            return;
        }
        // Keep the native event order even when two opposite arrows fall in
        // one frame. The last key should win a mid-transition reversal.
        let keys = ctx.input_mut(|i| {
            let mut keys = Vec::new();
            i.events.retain(|event| {
                if let egui::Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } = event
                    && modifiers.matches_logically(egui::Modifiers::NONE)
                    && matches!(
                        key,
                        Key::ArrowLeft
                            | Key::ArrowRight
                            | Key::ArrowUp
                            | Key::ArrowDown
                            | Key::Home
                            | Key::End
                            | Key::Backspace
                    )
                {
                    keys.push(*key);
                    false
                } else {
                    true
                }
            });
            keys
        });
        let before = self.interaction;
        for key in keys {
            self.navigate(key);
        }
        if before != self.interaction {
            ctx.memory_mut(|m| {
                if let Some(id) = m.focused() {
                    m.surrender_focus(id);
                }
            });
        }
        if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Delete))
            && let Some(path) = self.paths.get(self.carousel.target).cloned()
        {
            self.remove(path);
        }
        let selected_has_focus = ctx.memory(|memory| {
            memory.has_focus(egui::Id::unique("wall-image").with(self.carousel.target))
        });
        if (!ctx.egui_wants_keyboard_input() || selected_has_focus)
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::Enter))
        {
            #[cfg(debug_assertions)]
            if let Some(keydown) = platform::take_enter_time() {
                eprintln!(
                    "[apply] native Enter to UI handler: {:?}",
                    keydown.elapsed()
                );
            }
            self.apply();
        }
    }

    fn navigate(&mut self, key: Key) {
        let index = self.carousel.target;
        let last = self.paths.len().saturating_sub(1);
        match key {
            Key::Home => self.select(0),
            Key::End => self.select(last),
            Key::ArrowDown if !self.gallery.expanded() => self.set_gallery(true),
            Key::Backspace if self.gallery.expanded() => self.set_gallery(false),
            Key::ArrowUp if self.gallery.expanded() && self.gallery.active() => {
                self.set_gallery(false);
            }
            _ => {
                if self.gallery.expanded() {
                    if let Some(grid) = self.expanded_grid(Pos2::ZERO) {
                        let direction = match key {
                            Key::ArrowLeft => Direction::Left,
                            Key::ArrowRight => Direction::Right,
                            Key::ArrowUp => Direction::Up,
                            Key::ArrowDown => Direction::Down,
                            _ => return,
                        };
                        if let Some(next) = grid.move_selection(index, direction) {
                            self.select(next);
                        } else {
                            self.set_gallery(false);
                        }
                    }
                } else {
                    let delta = match key {
                        Key::ArrowLeft => -1,
                        Key::ArrowRight => 1,
                        _ => return,
                    };
                    if self.carousel.step(delta, self.paths.len()) {
                        self.interacted();
                    }
                }
            }
        }
    }

    fn wheel(&mut self, ctx: &egui::Context) {
        if self.state != OverlayState::Browsing || self.applying || egui::Popup::is_any_open(ctx) {
            return;
        }
        if self.gallery.expanded() {
            if let Some(grid) = self.expanded_grid(Pos2::ZERO) {
                let delta = ctx.input(|i| i.smooth_scroll_delta.y);
                self.scroll_gallery(&grid, grid.clamp_scroll(self.gallery_scroll - delta));
            }
            return;
        }
        let events = ctx.input(|i| i.events.clone());
        let mut changed = false;
        for event in events {
            if let egui::Event::MouseWheel { unit, delta, .. } = event {
                let distance = if delta.x.abs() > delta.y.abs() {
                    delta.x
                } else {
                    delta.y
                };
                let (distance, unit) = match unit {
                    egui::MouseWheelUnit::Line => (-distance, WheelUnit::Lines),
                    egui::MouseWheelUnit::Point => (-distance, WheelUnit::Points),
                    egui::MouseWheelUnit::Page => (-distance.signum(), WheelUnit::Lines),
                };
                changed |= self.carousel.wheel(distance, unit, self.paths.len());
            }
        }
        if changed {
            self.interacted();
        }
    }

    fn empty(&mut self, ui: &mut egui::Ui, rect: Rect) {
        ui.scope_builder(
            egui::UiBuilder::new()
                .max_rect(Rect::from_center_size(
                    rect.center(),
                    Vec2::new(450.0, 160.0),
                ))
                .layout(Layout::top_down(Align::Center)),
            |ui| {
                ui.add_space(12.0);
                ui.label(RichText::new("A different view.").size(25.0).color(INK));
                ui.add_space(9.0);
                ui.label(
                    RichText::new("Drop your wallpapers here.")
                        .size(14.0)
                        .color(MUTED),
                );
                ui.add_space(17.0);
                if ui.button("Choose images").clicked() {
                    self.pick();
                }
            },
        );
    }

    fn browse(&mut self, ui: &mut egui::Ui, outer: Rect) {
        let progress = self.gallery.progress();
        let grid = (progress > 0.0)
            .then(|| self.expanded_grid(outer.center()))
            .flatten();
        let collapsed = self.gallery_bounds.as_ref().map_or(outer.size(), |bounds| {
            bounds.sizes(self.ctx.pixels_per_point()).0
        });
        let carousel_bounds =
            Rect::from_center_size(outer.center(), collapsed).shrink2(Vec2::new(24.0, 0.0));
        let bounds = grid
            .as_ref()
            .map_or(carousel_bounds, |grid| {
                carousel_bounds.lerp_towards(&grid.viewport(), progress)
            })
            .intersect(outer);
        let previous_clip = ui.clip_rect();
        ui.set_clip_rect(previous_clip.intersect(bounds));
        let geometry = Geometry::new(carousel_bounds);
        let position = self.carousel.position;
        let start = (position.floor() as usize).saturating_sub(3);
        let end = ((position.ceil() as usize).saturating_add(4)).min(self.paths.len());
        let carousel_indices = start..end;
        let mut indices: Vec<_> = if progress < 1.0 {
            carousel_indices.clone().collect()
        } else {
            Vec::new()
        };
        if let Some(grid) = &grid {
            self.gallery_scroll = grid.clamp_scroll(self.gallery_scroll);
            indices.extend(
                grid.visible(self.gallery_scroll)
                    .filter(|index| progress == 1.0 || !carousel_indices.contains(index)),
            );
        }
        if progress < 1.0 {
            indices.sort_by(|a, b| {
                ((*b as f64 - position).abs()).total_cmp(&(*a as f64 - position).abs())
            });
        }
        let mut clicked = None;
        let mut apply = None;
        let mut remove = None;
        for index in indices {
            let distance = index as f64 - position;
            let carousel_rect = geometry.image(distance as f32);
            let in_carousel =
                carousel_indices.contains(&index) && carousel_rect.intersects(carousel_bounds);
            let (image_rect, opacity) = if let Some(grid) = &grid {
                let viewport = grid.viewport();
                let tile = grid.rect(index, self.gallery_scroll);
                if progress == 1.0 {
                    (tile, 1.0)
                } else if in_carousel {
                    // Keep an off-screen destination just beyond the clip, so
                    // collapsing after a long scroll still has a short path.
                    let y = tile.center().y.clamp(
                        viewport.top() - tile.height(),
                        viewport.bottom() + tile.height(),
                    );
                    let destination =
                        Rect::from_center_size(Pos2::new(tile.center().x, y), tile.size());
                    let alpha = if tile.intersects(viewport) {
                        1.0
                    } else {
                        1.0 - progress
                    };
                    (carousel_rect.lerp_towards(&destination, progress), alpha)
                } else {
                    let start = Rect::from_center_size(
                        tile.center() - Vec2::new(0.0, 16.0),
                        tile.size() * 0.92,
                    );
                    (start.lerp_towards(&tile, progress), progress * progress)
                }
            } else {
                (carousel_rect, 1.0)
            };
            if opacity <= 0.01 || !image_rect.intersects(bounds) {
                continue;
            }
            let selected = index == self.carousel.target;
            let weight = (1.0 - distance.abs().min(1.0)) as f32 * (1.0 - progress);
            let path = &self.paths[index];
            let name = filename(path);
            let response = ui.interact(
                image_rect.intersect(bounds),
                egui::Id::unique("wall-image").with(index),
                Sense::click(),
            );
            response.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::Role::Button,
                    !self.applying,
                    index == self.carousel.target,
                    &name,
                )
            });
            let mut painter = ui.painter().with_clip_rect(bounds);
            painter.multiply_opacity(opacity);
            if weight > 0.05 {
                painter.add(
                    egui::epaint::Shadow {
                        offset: [0, 9],
                        blur: 28,
                        spread: 1,
                        color: Color32::from_black_alpha((85.0 * weight) as u8),
                    }
                    .as_shape(image_rect, 7.0),
                );
            }
            match self.thumbnails.get(path) {
                ThumbState::Ready {
                    texture,
                    width,
                    height,
                } => {
                    let brightness = 196.0 + 59.0 * (weight + progress);
                    let tint = Color32::from_white_alpha((brightness * opacity) as u8);
                    let source = texture.size_vec2();
                    let fit = (image_rect.width() / source.x).min(image_rect.height() / source.y);
                    let fitted = if progress > 0.0 {
                        Rect::from_center_size(
                            image_rect.center(),
                            egui::lerp(image_rect.size()..=source * fit, progress),
                        )
                    } else {
                        image_rect
                    };
                    if progress > 0.0 && fitted.size() != image_rect.size() {
                        painter.rect_filled(image_rect, 7.0, Color32::from_black_alpha(85));
                    }
                    egui::Image::from_texture(&texture)
                        .uv(crop_uv(source, fitted.size()))
                        .tint(tint)
                        .corner_radius(7.0)
                        .paint_at(ui, fitted);
                    response.clone().on_hover_text(format!(
                        "{name}\n{width} × {height}\n{}",
                        if selected && (self.gallery.expanded() || self.carousel.is_settled()) {
                            "Double-click to set wallpaper"
                        } else {
                            "Click to preview"
                        }
                    ));
                }
                ThumbState::Pending => {
                    painter.rect_filled(
                        image_rect,
                        7.0,
                        Color32::from_rgba_unmultiplied(24, 29, 31, 130),
                    );
                    let c = image_rect.center();
                    painter.line_segment(
                        [c - Vec2::new(12.0, 0.0), c + Vec2::new(12.0, 0.0)],
                        Stroke::new(1.0, Color32::from_white_alpha(65)),
                    );
                }
                ThumbState::Failed(error) => {
                    painter.rect_filled(
                        image_rect,
                        7.0,
                        Color32::from_rgba_unmultiplied(20, 25, 27, 170),
                    );
                    painter.text(
                        image_rect.center(),
                        Align2::CENTER_CENTER,
                        "Image unavailable",
                        FontId::proportional(14.0),
                        MUTED,
                    );
                    response
                        .clone()
                        .on_hover_text(format!("{error}\nRight-click to remove this reference"));
                }
            }
            if weight > 0.0 || (selected && progress > 0.0) || response.has_focus() {
                painter.rect_stroke(
                    image_rect,
                    7.0,
                    Stroke::new(
                        1.0,
                        if response.has_focus() {
                            Color32::from_white_alpha(210)
                        } else if selected && progress > 0.0 {
                            Color32::from_white_alpha((60.0 * weight) as u8)
                                .lerp_to_gamma(ACCENT, progress)
                        } else {
                            Color32::from_white_alpha((60.0 * weight) as u8)
                        },
                    ),
                    StrokeKind::Inside,
                );
            }
            if progress < 1.0
                && self.current.as_ref().is_some_and(|p| same_path(path, p))
                && index != self.carousel.target
            {
                painter.circle_filled(
                    image_rect.center_bottom() + Vec2::new(0.0, 10.0),
                    2.3,
                    ACCENT.gamma_multiply(1.0 - progress),
                );
            }
            if response.clicked() && !response.double_clicked() && !self.applying {
                clicked = Some(index);
                response.surrender_focus();
            }
            if response.double_clicked()
                && !self.applying
                && selected
                && (self.gallery.expanded() || self.carousel.is_settled())
            {
                apply = Some(index);
            }
            response.context_menu(|ui| {
                if ui
                    .add_enabled(!self.applying, egui::Button::new("Set wallpaper"))
                    .clicked()
                {
                    apply = Some(index);
                    ui.close();
                }
                if ui.button("Remove from collection").clicked() {
                    remove = Some(path.clone());
                    ui.close();
                }
            });
        }
        if let Some(index) = clicked {
            self.select(index);
        }
        if let Some(path) = remove {
            self.remove(path);
        }
        if let Some(index) = apply {
            self.select(index);
            self.apply();
        }
        // Warm the destination of fast navigation without recreating any texture.
        if progress < 1.0
            && let Some(path) = self.paths.get(self.carousel.target)
        {
            let _ = self.thumbnails.get(path);
        }
        let opacity = (1.0 - (self.carousel.target as f64 - position).abs().min(1.0)) as f32
            * (1.0 - progress);
        if opacity > 0.02
            && let Some(path) = self.paths.get(self.carousel.target)
        {
            let label_rect = Rect::from_center_size(
                Pos2::new(carousel_bounds.center().x, geometry.label_y),
                Vec2::new(geometry.hero.x.min(carousel_bounds.width() - 130.0), 24.0),
            );
            let current = self.current.as_ref().is_some_and(|p| same_path(path, p));
            let dot_width = if current { 14.0 } else { 0.0 };
            let color = INK.gamma_multiply(opacity * 0.88);
            let mut job = egui::text::LayoutJob::simple(
                filename(path),
                FontId::proportional(13.0),
                color,
                label_rect.width() - dot_width,
            );
            job.wrap.max_rows = 1;
            job.wrap.break_anywhere = true;
            let galley = ui.fonts_mut(|fonts| fonts.layout_job(job));
            let x = label_rect.center().x - (galley.size().x + dot_width) * 0.5;
            if current {
                ui.painter().circle_filled(
                    Pos2::new(x + 3.0, label_rect.center().y),
                    2.3,
                    ACCENT.gamma_multiply(opacity),
                );
            }
            ui.painter().galley(
                Pos2::new(x + dot_width, label_rect.center().y - galley.size().y * 0.5),
                galley,
                color,
            );
            ui.interact(label_rect, egui::Id::unique("image-name"), Sense::hover())
                .on_hover_text(if current {
                    format!("{} · On your desktop", filename(path))
                } else {
                    filename(path)
                });
        }
        ui.set_clip_rect(previous_clip);
        if let Some(grid) = grid
            && grid.max_scroll() > 0.0
        {
            let track = grid.viewport();
            let height = track.height();
            let thumb = (height * height / (height + grid.max_scroll())).max(24.0);
            let offset = (height - thumb) * (self.gallery_scroll / grid.max_scroll());
            let rect = Rect::from_min_size(
                Pos2::new(track.right() + 8.0, track.top() + offset),
                Vec2::new(2.0, thumb),
            );
            if rect.intersects(outer) {
                ui.painter().rect_filled(
                    rect,
                    1.0,
                    Color32::from_white_alpha((60.0 * progress) as u8),
                );
            }
        }
        if progress == 0.0 && !self.gallery.expanded() {
            self.thumbnails.set_gallery(false);
        }
    }

    fn actions(&mut self, ui: &mut egui::Ui, bounds: Rect) {
        let rect = Rect::from_min_size(
            Pos2::new(bounds.right() - 58.0, bounds.bottom() - 45.0),
            Vec2::new(34.0, 28.0),
        );
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            let inner = ui.menu_button("     ", |ui| {
                if self.state == OverlayState::Browsing && !self.paths.is_empty() {
                    let expanded = self.gallery.expanded();
                    let label = if expanded {
                        "Carousel     Backspace"
                    } else {
                        "Gallery     ↓"
                    };
                    if ui.button(label).clicked() {
                        self.set_gallery(!expanded);
                        ui.close();
                    }
                }
                if ui.button("Add images…       Ctrl+O").clicked() {
                    self.pick();
                    ui.close();
                }
                if ui.button("Preferences…      Ctrl+,").clicked() {
                    self.open_preferences();
                    ui.close();
                }
                if let Some(index) = self.imported_start
                    && ui.button("View imported images").clicked()
                {
                    self.select(index);
                    self.imported_start = None;
                    ui.close();
                }
                ui.separator();
                if ui.button("Quit wall.        Ctrl+Q").clicked() {
                    self.quitting = true;
                    ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
                }
            });
            inner.response.widget_info(|| {
                egui::WidgetInfo::labeled(egui::Role::Button, true, "More actions")
            });
            let center = inner.response.rect.center();
            for x in [-5.0, 0.0, 5.0] {
                ui.painter()
                    .circle_filled(center + Vec2::new(x, 0.0), 1.35, MUTED);
            }
            inner.response.on_hover_text("Add images and preferences");
        });
    }

    fn open_preferences(&mut self) {
        self.draft = self.settings.clone();
        self.startup = None;
        self.state = OverlayState::Preferences;
        self.send(Command::ReadStartup);
    }

    fn preferences(&mut self, ui: &mut egui::Ui, bounds: Rect) {
        let width = 590.0_f32.min(bounds.width() - 48.0);
        let height = 360.0_f32.min(bounds.height() - 62.0);
        let rect = Rect::from_center_size(bounds.center(), Vec2::new(width, height));
        ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("Preferences").size(21.0));
                ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
                    if ui.button("Back").clicked() {
                        self.state = OverlayState::Browsing;
                    }
                });
            });
            egui::ScrollArea::vertical()
                .max_height((height - 40.0).max(0.0))
                .show(ui, |ui| {
                    ui.add_space(12.0);
                    ui.horizontal(|ui| {
                        ui.add_sized([122.0, 26.0], egui::Label::new("Global shortcut"));
                        ui.checkbox(&mut self.draft.hotkey.ctrl, "Ctrl");
                        ui.checkbox(&mut self.draft.hotkey.alt, "Alt");
                        ui.checkbox(&mut self.draft.hotkey.shift, "Shift");
                        egui::ComboBox::from_id_salt("shortcut-key")
                            .selected_text(
                                char::from_u32(self.draft.hotkey.key)
                                    .unwrap_or('W')
                                    .to_string(),
                            )
                            .width(48.0)
                            .show_ui(ui, |ui| {
                                for key in (b'A'..=b'Z').chain(b'0'..=b'9') {
                                    ui.selectable_value(
                                        &mut self.draft.hotkey.key,
                                        u32::from(key),
                                        char::from(key).to_string(),
                                    );
                                }
                            });
                    });
                    ui.add_space(11.0);
                    ui.horizontal(|ui| {
                        ui.add_sized([122.0, 26.0], egui::Label::new("Surface"));
                        ui.selectable_value(&mut self.draft.glass, true, "Glass");
                        ui.selectable_value(&mut self.draft.glass, false, "Solid");
                    });
                    ui.add_space(7.0);
                    ui.label(
                        RichText::new(self.composition.description())
                            .size(12.0)
                            .color(MUTED),
                    );
                    if self.reduced_motion {
                        ui.label(
                            RichText::new("Windows reduced motion is enabled.")
                                .size(12.0)
                                .color(MUTED),
                        );
                    }
                    ui.horizontal(|ui| {
                        ui.add_sized([122.0, 26.0], egui::Label::new("Tray icon"));
                        let choices = [
                            (TrayAppearance::Automatic, "Automatic"),
                            (TrayAppearance::Light, "Light symbol"),
                            (TrayAppearance::Dark, "Dark symbol"),
                        ];
                        let selected = choices
                            .iter()
                            .find(|(mode, _)| *mode == self.draft.tray_appearance)
                            .map_or("Automatic", |(_, label)| *label);
                        egui::ComboBox::from_id_salt("tray-appearance")
                            .selected_text(selected)
                            .show_ui(ui, |ui| {
                                for (mode, label) in choices {
                                    ui.selectable_value(
                                        &mut self.draft.tray_appearance,
                                        mode,
                                        label,
                                    );
                                }
                            });
                    });
                    ui.horizontal(|ui| {
                        ui.add_sized([122.0, 26.0], egui::Label::new("Startup"));
                        ui.add_enabled(
                            self.startup.is_some(),
                            egui::Checkbox::new(&mut self.draft_startup, "Start with Windows"),
                        );
                    });
                    ui.add_space(14.0);
                    if ui
                        .add_enabled(
                            self.pending_edits == 0,
                            egui::Button::new("Save preferences"),
                        )
                        .clicked()
                    {
                        if !(self.draft.hotkey.alt || self.draft.hotkey.ctrl) {
                            self.tell("Include Alt or Ctrl in the shortcut");
                        } else {
                            match self.platform.set_hotkey(self.draft.hotkey) {
                                Ok(()) => {
                                    self.pending_edits += 1;
                                    let startup = self
                                        .startup
                                        .filter(|current| *current != self.draft_startup)
                                        .map(|_| self.draft_startup);
                                    self.send(Command::Settings(self.draft.clone(), startup));
                                    self.state = OverlayState::Browsing;
                                }
                                Err(error) => self.tell(error),
                            }
                        }
                    }
                    ui.add_space(8.0);
                    ui.separator();
                    ui.horizontal(|ui| {
                        let mark = self.wordmark.get_or_insert_with(|| {
                            let (rgba, width, height) =
                                brand_image(include_bytes!("../assets/wordmark.png"));
                            ui.ctx().load_texture(
                                "wall-wordmark",
                                egui::ColorImage::from_rgba_unmultiplied(
                                    [width as usize, height as usize],
                                    &rgba,
                                ),
                                egui::TextureOptions::LINEAR,
                            )
                        });
                        let size = mark.size_vec2();
                        ui.add(
                            egui::Image::from_texture((mark.id(), size * (56.0 / size.x)))
                                .alt_text("wall."),
                        );
                        ui.label(
                            RichText::new(concat!(
                                "v",
                                env!("CARGO_PKG_VERSION"),
                                " · Wallpaper switcher for Windows"
                            ))
                            .size(12.0)
                            .color(MUTED),
                        );
                    });
                });
        });
    }

    fn overlays(&mut self, ui: &mut egui::Ui, bounds: Rect) {
        let message = if self.applying && self.reveal.opening {
            Some("Setting wallpaper…".to_owned())
        } else if let Some((message, _)) = &self.notice {
            Some(message.clone())
        } else if self.pending_edits > 0 {
            Some("Adding images…".to_owned())
        } else {
            None
        };
        if let Some(message) = message {
            let rect = Rect::from_center_size(
                Pos2::new(bounds.center().x, bounds.top() + 23.0),
                Vec2::new((bounds.width() - 100.0).min(780.0), 24.0),
            );
            ui.scope_builder(
                egui::UiBuilder::new()
                    .max_rect(rect)
                    .layout(Layout::top_down(Align::Center)),
                |ui| {
                    ui.add(
                        egui::Label::new(RichText::new(&message).size(12.0).color(INK)).truncate(),
                    )
                    .on_hover_text(message);
                },
            );
        }
        if ui.ctx().input(|i| !i.raw.hovered_files.is_empty()) {
            let painter = ui.ctx().layer_painter(egui::LayerId::new(
                egui::Order::Foreground,
                egui::Id::unique("file-drop"),
            ));
            painter.rect_stroke(
                bounds.shrink(9.0),
                12.0,
                Stroke::new(1.5, ACCENT),
                StrokeKind::Inside,
            );
            let anchor = Pos2::new(bounds.center().x, bounds.top() + 25.0);
            let text_rect = Rect::from_center_size(anchor, Vec2::new(220.0, 30.0));
            painter.rect_filled(
                text_rect,
                6.0,
                Color32::from_rgba_unmultiplied(20, 25, 25, 235),
            );
            painter.text(
                anchor,
                Align2::CENTER_CENTER,
                "Drop to add wallpapers",
                FontId::proportional(13.0),
                INK,
            );
        }
    }
}

impl eframe::App for WallApp {
    fn logic(&mut self, ctx: &egui::Context, _: &mut eframe::Frame) {
        if self.first {
            self.first = false;
            if !self.background {
                self.show(ctx);
            }
        }
        self.events(ctx);
        if self.state.visible() {
            self.thumbnails.poll();
        }
    }

    fn ui(&mut self, ui: &mut egui::Ui, _: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if ctx.input(|i| i.viewport().close_requested()) && !self.quitting {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
            self.hide(&ctx);
        }
        if !self.state.visible() {
            return;
        }
        // Advance the previous target up to this input event. Idle wall-clock
        // time must not be applied to a target that was only just requested.
        let now = Instant::now();
        let dt = now.duration_since(self.last_frame).as_secs_f64();
        self.last_frame = now;
        let gallery_progress = self.gallery.progress();
        if self.gallery.advance(dt, self.reduced_motion) {
            ctx.request_repaint();
        }
        if self.state == OverlayState::Browsing && self.carousel.advance(dt, self.reduced_motion) {
            ctx.request_repaint();
        }
        if self.reveal.opening {
            self.keyboard(&ctx);
        }
        if !self.state.visible() {
            return;
        }
        if gallery_progress != self.gallery.progress() {
            self.resize_gallery();
        }
        if self.reveal.opening {
            self.wheel(&ctx);
        }
        let amount = self.reveal.value(Instant::now());
        if !self.reveal.opening && amount == 0.0 {
            self.hide_now(&ctx);
            return;
        }
        if !self.reveal.opening || amount < 1.0 {
            ctx.request_repaint();
        }
        if self.reduced_motion {
            self.carousel.advance(0.0, true);
        }
        #[cfg(debug_assertions)]
        debug_backdrop(&ctx);
        if self
            .notice
            .as_ref()
            .is_some_and(|(_, time)| time.elapsed() >= Duration::from_secs(7))
        {
            self.notice = None;
        }
        // Drop events are handled before any dismissal logic. Losing focus alone
        // has no effect: Explorer remains usable to prepare and begin a drag.
        let drops: Vec<_> = ctx.input(|i| {
            i.raw
                .dropped_files
                .iter()
                .map(|f| f.path().to_path_buf())
                .collect()
        });
        if !drops.is_empty() {
            self.pending_edits += 1;
            self.send(Command::Import(
                drops,
                worker::ImportContext {
                    epoch: self.interaction,
                    reveal: self.carousel.is_settled(),
                },
            ));
        }
        let bounds = ui.max_rect();
        let fill = if self.composition.is_acrylic() {
            Color32::from_black_alpha(if self.state == OverlayState::Preferences {
                100
            } else {
                64
            })
        } else {
            Color32::from_rgb(24, 28, 31)
        };
        // DWM clips the whole HWND and draws its outer border in physical
        // pixels. A second logical-radius outline exposes an untinted crescent.
        ui.painter().rect_filled(bounds, 0.0, fill);
        if !self.reveal.opening {
            ui.disable();
        }
        ui.set_opacity(0.65 + 0.35 * amount);
        match self.state {
            OverlayState::Browsing if self.paths.is_empty() => self.empty(ui, bounds),
            OverlayState::Browsing => self.browse(ui, bounds),
            OverlayState::Preferences => self.preferences(ui, bounds),
            _ => {}
        }
        self.actions(ui, bounds);
        self.overlays(ui, bounds);
    }

    fn clear_color(&self, _: &egui::Visuals) -> [f32; 4] {
        [0.0; 4]
    }
}

/// Same-process diagnostic surface: shows whether the actual desktop backdrop
/// is sampled and blurred. Never compiled into the delivered release.
#[cfg(debug_assertions)]
fn debug_backdrop(ctx: &egui::Context) {
    let Ok(mode) = std::env::var("WALL_GLASS_PROBE") else {
        return;
    };
    let id = egui::ViewportId::from_hash_of("backdrop-probe");
    let toggle = egui::Id::unique("dark-probe");
    if ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, Key::F9)) {
        ctx.data_mut(|data| {
            let dark = data.get_temp::<bool>(toggle).unwrap_or(false);
            data.insert_temp(toggle, !dark);
        });
        ctx.request_repaint_of(id);
    }
    let dark = ctx.data(|data| data.get_temp::<bool>(toggle).unwrap_or(false));
    let origin = ctx
        .input(|input| input.viewport().outer_rect.map(|r| r.min))
        .unwrap_or_default();
    ctx.show_viewport_deferred(
        id,
        egui::ViewportBuilder::default()
            .with_title("wall. backdrop test")
            .with_inner_size([1500.0, 850.0])
            .with_position(origin - Vec2::splat(80.0))
            .with_active(false)
            .with_window_level(egui::WindowLevel::Normal),
        move |ctx, _| {
            egui::CentralPanel::default().show(ctx, |ui| {
                let r = ui.max_rect();
                if let Some(color) = match mode.as_str() {
                    "light" => Some(Color32::from_gray(if dark { 17 } else { 244 })),
                    "dark" => Some(Color32::from_gray(17)),
                    _ => None,
                } {
                    ui.painter().rect_filled(r, 0.0, color);
                    return;
                }
                let half = r.width() * 0.5;
                for (x, color) in [
                    (0.0, Color32::from_rgb(196, 109, 44)),
                    (half, Color32::from_rgb(42, 127, 190)),
                ] {
                    ui.painter().rect_filled(
                        Rect::from_min_size(r.min + Vec2::new(x, 0.0), Vec2::new(half, r.height())),
                        0.0,
                        color,
                    );
                }
                for x in (0..r.width() as usize).step_by(30) {
                    let a = r.min + Vec2::new(x as f32, 0.0);
                    ui.painter().line_segment(
                        [a, a + Vec2::new(0.0, r.height())],
                        Stroke::new(2.0, Color32::WHITE),
                    );
                }
                ui.painter().text(
                    r.center_top() + Vec2::new(0.0, 24.0),
                    Align2::CENTER_TOP,
                    "wall. — backdrop test",
                    FontId::proportional(22.0),
                    Color32::WHITE,
                );
            });
        },
    );
}

struct Geometry {
    center: Pos2,
    hero: Vec2,
    stride: f32,
    label_y: f32,
}
impl Geometry {
    fn new(bounds: Rect) -> Self {
        let height = (bounds.height() - 100.0).clamp(90.0, 382.0);
        let width = (height * 16.0 / 9.0).min(bounds.width() * 0.61);
        let hero = Vec2::new(width, width * 9.0 / 16.0);
        let center = Pos2::new(bounds.center().x, bounds.center().y - 5.0);
        Self {
            center,
            hero,
            stride: hero.x * 0.82,
            label_y: center.y + hero.y * 0.5 + 24.0,
        }
    }
    fn image(&self, distance: f32) -> Rect {
        let recession = distance.abs().min(1.0);
        let scale = 1.0 - 0.23 * recession;
        Rect::from_center_size(
            self.center + Vec2::new(distance * self.stride, 0.0),
            self.hero * scale,
        )
    }
}

fn filename(path: &Path) -> String {
    path.file_stem()
        .unwrap_or(path.as_os_str())
        .to_string_lossy()
        .into_owned()
}
fn crop_uv(image: Vec2, frame: Vec2) -> Rect {
    let scale = (frame.x / image.x).max(frame.y / image.y);
    Rect::from_center_size(Pos2::new(0.5, 0.5), frame / (image * scale))
}
fn style(ctx: &egui::Context) {
    ctx.set_theme(egui::ThemePreference::Dark);
    let mut style = egui::Style {
        visuals: egui::Visuals::dark(),
        ..Default::default()
    };
    style.visuals.override_text_color = Some(INK);
    style.visuals.panel_fill = Color32::TRANSPARENT;
    style.visuals.extreme_bg_color = Color32::from_rgb(19, 23, 25);
    style.visuals.widgets.inactive.bg_fill = Color32::from_rgba_unmultiplied(49, 55, 57, 210);
    style.visuals.widgets.inactive.weak_bg_fill = Color32::TRANSPARENT;
    style.visuals.widgets.hovered.bg_fill = Color32::from_rgb(57, 64, 66);
    style.visuals.selection.bg_fill = Color32::from_rgb(68, 84, 74);
    style.visuals.selection.stroke = Stroke::new(1.0, INK);
    style.spacing.item_spacing = Vec2::new(10.0, 8.0);
    style.spacing.button_padding = Vec2::new(12.0, 7.0);
    style.animation_time = 0.0;
    style.scroll_animation = egui::style::ScrollAnimation::none();
    ctx.set_style_of(egui::Theme::Dark, style);
}

pub fn icon() -> egui::IconData {
    let (rgba, width, height) = brand_image(include_bytes!("../assets/window-icon.png"));
    egui::IconData {
        rgba,
        width,
        height,
    }
}

fn brand_image(bytes: &[u8]) -> (Vec<u8>, u32, u32) {
    let rgba = image::load_from_memory_with_format(bytes, image::ImageFormat::Png)
        .expect("bundled branding is a valid PNG")
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    (rgba.into_raw(), width, height)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reveal_reverses_continuously_and_finishes_at_rest() {
        let start = Instant::now();
        let mut reveal = Reveal::hidden(start);
        reveal.target(true, start, false);
        let reverse = start + Duration::from_millis(24);
        let middle = reveal.value(reverse);
        assert!(middle > 0.0 && middle < 1.0);
        reveal.target(false, reverse, false);
        assert_eq!(reveal.value(reverse), middle);
        let reopen = reverse + Duration::from_millis(8);
        let fading = reveal.value(reopen);
        assert!(fading < middle && fading > 0.0);
        reveal.target(true, reopen, false);
        assert!((reveal.value(reopen) - fading).abs() < 0.00001);
        assert_eq!(reveal.value(reopen + Duration::from_millis(110)), 1.0);
        reveal.target(false, reopen, true);
        assert_eq!(reveal.value(reopen), 0.0);
        reveal.target(true, reopen, true);
        assert_eq!(reveal.value(reopen), 1.0);
    }
    #[test]
    fn center_and_recession_are_continuous_and_exact() {
        let geometry = Geometry::new(Rect::from_min_size(Pos2::ZERO, Vec2::new(1160.0, 500.0)));
        assert_eq!(geometry.image(0.0).center(), geometry.center);
        assert!(geometry.image(1.0).width() < geometry.image(0.0).width());
        assert!((geometry.image(0.4999).width() - geometry.image(0.5001).width()).abs() < 0.1);
        assert_eq!(geometry.image(-1.0).size(), geometry.image(1.0).size());
    }
    #[test]
    fn crop_preserves_aspect_and_stays_inside_image() {
        let uv = crop_uv(Vec2::new(4000.0, 1000.0), Vec2::new(640.0, 360.0));
        assert!((uv.height() - 1.0).abs() < 0.001 && uv.width() < 1.0 && uv.left() > 0.0);
    }
}
