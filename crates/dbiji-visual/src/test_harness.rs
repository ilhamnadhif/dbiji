//! Headless egui driver for UI tests: runs frames (including tessellation)
//! and simulates pointer and keyboard input.

use egui::{Context, Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Ui, Vec2, vec2};

pub(crate) struct Harness {
    pub ctx: Context,
    time: f64,
    size: Vec2,
    modifiers: Modifiers,
}

impl Harness {
    pub fn new(width: f32, height: f32) -> Self {
        let ctx = Context::default();
        let mut fonts = egui::FontDefinitions::default();
        egui_phosphor::add_to_fonts(&mut fonts, egui_phosphor::Variant::Regular);
        ctx.set_fonts(fonts);
        Harness {
            ctx,
            time: 0.0,
            size: vec2(width, height),
            modifiers: Modifiers::NONE,
        }
    }

    /// Runs one frame with `events`; `f` builds the UI inside a central panel.
    pub fn frame(&mut self, events: Vec<Event>, f: &mut dyn FnMut(&mut Ui)) {
        self.time += 0.05;
        let mut events = events;
        events.insert(0, Event::ModifiersChanged(self.modifiers));
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, self.size)),
            time: Some(self.time),
            events,
            ..Default::default()
        };
        let mut out = self.ctx.run_ui(input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| f(ui));
        });
        out.textures_delta.clear();
        let prims = self
            .ctx
            .tessellate(std::mem::take(&mut out.shapes), out.pixels_per_point);
        assert!(prims.iter().all(|p| match &p.primitive {
            egui::epaint::Primitive::Mesh(m) => m.is_valid(),
            _ => true,
        }));
    }

    pub fn idle(&mut self, frames: usize, f: &mut dyn FnMut(&mut Ui)) {
        for _ in 0..frames {
            self.frame(vec![], f);
        }
    }

    pub fn set_modifiers(&mut self, m: Modifiers) {
        self.modifiers = m;
    }

    fn button(&self, pos: Pos2, button: PointerButton, pressed: bool) -> Event {
        Event::PointerButton {
            pos,
            button,
            pressed,
            modifiers: self.modifiers,
        }
    }

    pub fn hover(&mut self, pos: Pos2, f: &mut dyn FnMut(&mut Ui)) {
        self.frame(vec![Event::PointerMoved(pos)], f);
        self.frame(vec![], f);
    }

    pub fn click_with(&mut self, pos: Pos2, button: PointerButton, f: &mut dyn FnMut(&mut Ui)) {
        self.frame(vec![Event::PointerMoved(pos)], f);
        let press = self.button(pos, button, true);
        self.frame(vec![press], f);
        let release = self.button(pos, button, false);
        self.frame(vec![release], f);
    }

    pub fn click(&mut self, pos: Pos2, f: &mut dyn FnMut(&mut Ui)) {
        self.click_with(pos, PointerButton::Primary, f);
    }

    pub fn double_click(&mut self, pos: Pos2, f: &mut dyn FnMut(&mut Ui)) {
        self.frame(vec![Event::PointerMoved(pos)], f);
        for _ in 0..2 {
            let press = self.button(pos, PointerButton::Primary, true);
            self.frame(vec![press], f);
            let release = self.button(pos, PointerButton::Primary, false);
            self.frame(vec![release], f);
        }
    }

    pub fn drag(&mut self, from: Pos2, to: Pos2, f: &mut dyn FnMut(&mut Ui)) {
        self.frame(vec![Event::PointerMoved(from)], f);
        let press = self.button(from, PointerButton::Primary, true);
        self.frame(vec![press], f);
        let steps = 8;
        for i in 1..=steps {
            let p = from + (to - from) * (i as f32 / steps as f32);
            self.frame(vec![Event::PointerMoved(p)], f);
        }
        let release = self.button(to, PointerButton::Primary, false);
        self.frame(vec![release], f);
    }

    pub fn key(&mut self, key: Key, f: &mut dyn FnMut(&mut Ui)) {
        let m = self.modifiers;
        self.frame(
            vec![Event::Key {
                key,
                physical_key: None,
                pressed: true,
                repeat: false,
                modifiers: m,
            }],
            f,
        );
        self.frame(
            vec![Event::Key {
                key,
                physical_key: None,
                pressed: false,
                repeat: false,
                modifiers: m,
            }],
            f,
        );
    }

    pub fn scroll(&mut self, at: Pos2, delta: Vec2, f: &mut dyn FnMut(&mut Ui)) {
        self.frame(vec![Event::PointerMoved(at)], f);
        let m = self.modifiers;
        self.frame(
            vec![Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta,
                phase: egui::TouchPhase::Move,
                modifiers: m,
            }],
            f,
        );
        self.idle(5, f);
    }
}
