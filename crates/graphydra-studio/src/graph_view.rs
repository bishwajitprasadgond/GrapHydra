//! Interactive force-directed graph canvas.

use std::collections::{BTreeMap, HashMap};

use eframe::egui::{
    self, Align2, Color32, CornerRadius, FontId, Pos2, Rect, Response, Sense, Shape, Stroke, StrokeKind, Ui, Vec2,
};
use graphydra_core::{Edge, Node, Value};

use crate::theme::Palette;

/// Upper bound on nodes shown at once; keeps layout O(n²) interactive.
pub const MAX_VISIBLE_NODES: usize = 1500;

#[derive(Clone)]
pub struct VNode {
    pub node: Node,
    pos: Vec2,
    vel: Vec2,
    pinned: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Selection {
    Node(String),
    Edge(String),
}

/// What the user asked the canvas to do this frame (handled by the app).
pub enum CanvasAction {
    Select,
    Expand(String),
    DeleteNode(String),
    ConnectFrom(String),
}

pub struct GraphView {
    nodes: BTreeMap<String, VNode>,
    order: Vec<String>,
    edges: BTreeMap<String, Edge>,
    pub selected: Option<Selection>,
    pan: Vec2,
    zoom: f32,
    dragging: Option<String>,
    temperature: f32,
    pub show_edge_labels: bool,
    pub truncated: usize,
    /// Keep the drawing fitted while the layout settles (until the user pans or zooms).
    auto_fit: bool,
    seed: u64,
    /// Where the canvas was drawn last frame (for screen-position queries).
    last_rect: Rect,
}

impl Default for GraphView {
    fn default() -> Self {
        GraphView {
            nodes: BTreeMap::new(),
            order: Vec::new(),
            edges: BTreeMap::new(),
            selected: None,
            pan: Vec2::ZERO,
            zoom: 1.0,
            dragging: None,
            temperature: 1.0,
            show_edge_labels: true,
            truncated: 0,
            auto_fit: false,
            seed: 0x2545_F491_4F6C_DD1D,
            last_rect: Rect::NOTHING,
        }
    }
}

/// Text shown under a node: the first "name-like" property, else the id.
pub fn caption(node: &Node) -> String {
    for key in ["name", "title", "hostname", "label", "family", "value"] {
        if let Some(Value::String(s)) = node.properties.get(key) {
            return s.clone();
        }
    }
    node.id.clone()
}

impl GraphView {
    pub fn node_count(&self) -> usize {
        self.nodes.len()
    }

    pub fn edge_count(&self) -> usize {
        self.edges.len()
    }

    pub fn clear(&mut self) {
        *self = GraphView { show_edge_labels: self.show_edge_labels, ..GraphView::default() };
    }

    fn rand(&mut self) -> f32 {
        self.seed ^= self.seed >> 12;
        self.seed ^= self.seed << 25;
        self.seed ^= self.seed >> 27;
        (self.seed.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Add or refresh a node. New nodes appear near `near` (or randomly).
    pub fn add_node(&mut self, node: Node, near: Option<Vec2>) {
        if let Some(v) = self.nodes.get_mut(&node.id) {
            v.node = node;
            return;
        }
        if self.nodes.len() >= MAX_VISIBLE_NODES {
            self.truncated += 1;
            return;
        }
        let spread = 40.0 + 8.0 * (self.nodes.len() as f32).sqrt();
        let base = near.unwrap_or(Vec2::ZERO);
        let pos = base + Vec2::new(self.rand() - 0.5, self.rand() - 0.5) * spread * 2.0;
        self.order.push(node.id.clone());
        self.nodes.insert(node.id.clone(), VNode { node, pos, vel: Vec2::ZERO, pinned: false });
        self.temperature = 1.0;
    }

    /// Add an edge if both endpoints are shown.
    pub fn add_edge(&mut self, edge: Edge) {
        if self.nodes.contains_key(&edge.source) && self.nodes.contains_key(&edge.target) {
            self.edges.insert(edge.id.clone(), edge);
            self.temperature = self.temperature.max(0.5);
        }
    }

    /// Where a node was drawn on screen last frame.
    #[cfg(test)]
    pub fn screen_pos(&self, id: &str) -> Option<Pos2> {
        self.nodes.get(id).map(|n| self.to_screen(self.last_rect, n.pos))
    }

    pub fn position_of(&self, id: &str) -> Option<Vec2> {
        self.nodes.get(id).map(|n| n.pos)
    }

    pub fn contains_node(&self, id: &str) -> bool {
        self.nodes.contains_key(id)
    }

    pub fn node_ids(&self) -> Vec<String> {
        self.order.clone()
    }

    pub fn remove_node(&mut self, id: &str) {
        self.nodes.remove(id);
        self.order.retain(|n| n != id);
        self.edges.retain(|_, e| e.source != id && e.target != id);
        if matches!(&self.selected, Some(Selection::Node(n)) if n == id) {
            self.selected = None;
        }
    }

    pub fn remove_edge(&mut self, id: &str) {
        self.edges.remove(id);
        if matches!(&self.selected, Some(Selection::Edge(e)) if e == id) {
            self.selected = None;
        }
    }

    pub fn selected_node(&self) -> Option<&Node> {
        match &self.selected {
            Some(Selection::Node(id)) => self.nodes.get(id).map(|v| &v.node),
            _ => None,
        }
    }

    pub fn selected_edge(&self) -> Option<&Edge> {
        match &self.selected {
            Some(Selection::Edge(id)) => self.edges.get(id),
            _ => None,
        }
    }

    pub fn relayout(&mut self) {
        for id in self.order.clone() {
            let p = Vec2::new(self.rand() - 0.5, self.rand() - 0.5) * 400.0;
            if let Some(v) = self.nodes.get_mut(&id) {
                v.pos = p;
                v.pinned = false;
            }
        }
        self.temperature = 1.0;
    }

    /// Fit now and keep fitting until the layout settles.
    pub fn request_fit(&mut self) {
        self.auto_fit = true;
    }

    pub fn fit(&mut self, viewport: Rect) {
        if self.nodes.is_empty() {
            self.pan = Vec2::ZERO;
            self.zoom = 1.0;
            return;
        }
        let (mut lo, mut hi) = (Vec2::splat(f32::MAX), Vec2::splat(f32::MIN));
        for v in self.nodes.values() {
            lo = lo.min(v.pos);
            hi = hi.max(v.pos);
        }
        let size = (hi - lo).max(Vec2::splat(1.0)) + Vec2::splat(120.0);
        self.zoom = (viewport.width() / size.x).min(viewport.height() / size.y).clamp(0.05, 3.0);
        self.pan = -(lo + hi) / 2.0 * self.zoom;
    }

    /// One step of a Fruchterman–Reingold style simulation.
    fn simulate(&mut self) {
        if self.temperature < 0.01 || self.nodes.is_empty() {
            return;
        }
        let ids = &self.order;
        let index: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, id)| (id.as_str(), i)).collect();
        let pos: Vec<Vec2> = ids.iter().map(|id| self.nodes[id].pos).collect();
        let mut force = vec![Vec2::ZERO; pos.len()];
        let k = 70.0f32;
        for i in 0..pos.len() {
            for j in (i + 1)..pos.len() {
                let mut d = pos[i] - pos[j];
                let mut dist2 = d.length_sq();
                if dist2 < 0.01 {
                    d = Vec2::new(0.1 * (i as f32 - j as f32).signum(), 0.1);
                    dist2 = 0.02;
                }
                if dist2 > 600.0 * 600.0 {
                    continue;
                }
                let f = d * (k * k / dist2);
                force[i] += f;
                force[j] -= f;
            }
        }
        for e in self.edges.values() {
            if let (Some(&a), Some(&b)) = (index.get(e.source.as_str()), index.get(e.target.as_str())) {
                if a == b {
                    continue;
                }
                let d = pos[b] - pos[a];
                let dist = d.length().max(0.1);
                let f = d / dist * (dist * dist / k) * 0.6;
                force[a] += f;
                force[b] -= f;
            }
        }
        let t = self.temperature;
        for (i, id) in ids.iter().enumerate() {
            let v = self.nodes.get_mut(id).expect("ordered id exists");
            if v.pinned || self.dragging.as_deref() == Some(id) {
                v.vel = Vec2::ZERO;
                continue;
            }
            // Gentle gravity keeps disconnected components on screen.
            let f = force[i] - v.pos * 0.02;
            v.vel = (v.vel + f * 0.02) * 0.6;
            let step = v.vel.length();
            let max = 25.0 * t + 0.5;
            if step > max {
                v.vel *= max / step;
            }
            v.pos += v.vel;
        }
        self.temperature *= 0.985;
    }

    fn to_screen(&self, rect: Rect, p: Vec2) -> Pos2 {
        rect.center() + self.pan + p * self.zoom
    }

    fn to_world(&self, rect: Rect, p: Pos2) -> Vec2 {
        (p - rect.center() - self.pan) / self.zoom
    }

    fn hit_node(&self, rect: Rect, p: Pos2) -> Option<String> {
        let r = (11.0 * self.zoom.sqrt()).max(6.0);
        self.order.iter().rev().find(|id| self.to_screen(rect, self.nodes[*id].pos).distance(p) <= r + 3.0).cloned()
    }

    fn hit_edge(&self, rect: Rect, p: Pos2) -> Option<String> {
        let mut best: Option<(f32, String)> = None;
        for (id, e) in &self.edges {
            let (Some(a), Some(b)) = (self.nodes.get(&e.source), self.nodes.get(&e.target)) else { continue };
            let (a, b) = (self.to_screen(rect, a.pos), self.to_screen(rect, b.pos));
            let ab = b - a;
            let t = ((p - a).dot(ab) / ab.length_sq().max(1e-3)).clamp(0.0, 1.0);
            let d = (a + ab * t).distance(p);
            if d < 5.0 && best.as_ref().is_none_or(|(bd, _)| d < *bd) {
                best = Some((d, id.clone()));
            }
        }
        best.map(|(_, id)| id)
    }

    /// Draw the canvas and handle input. Returns user actions.
    pub fn ui(&mut self, ui: &mut Ui, pal: &Palette, label_slots: &BTreeMap<String, usize>) -> Vec<CanvasAction> {
        let mut actions = Vec::new();
        let (response, painter) = ui.allocate_painter(ui.available_size(), Sense::click_and_drag());
        let rect = response.rect;
        self.last_rect = rect;
        painter.rect_filled(rect, CornerRadius::ZERO, pal.surface);

        self.handle_input(ui, &response, rect, &mut actions);
        for _ in 0..3 {
            self.simulate();
        }
        if self.auto_fit {
            self.fit(rect);
            if self.temperature < 0.02 {
                self.auto_fit = false;
            }
        }
        if self.temperature >= 0.01 || self.dragging.is_some() {
            ui.ctx().request_repaint();
        }

        let node_r = (9.0 * self.zoom.sqrt()).clamp(4.0, 16.0);
        let font = FontId::proportional((11.0 * self.zoom.sqrt()).clamp(9.0, 14.0));
        let color_of =
            |n: &Node| -> Color32 { pal.label_color(n.labels.iter().next().and_then(|l| label_slots.get(l))) };

        // Edges first (under nodes).
        for (id, e) in &self.edges {
            let (Some(a), Some(b)) = (self.nodes.get(&e.source), self.nodes.get(&e.target)) else { continue };
            let selected = matches!(&self.selected, Some(Selection::Edge(s)) if s == id);
            let stroke =
                Stroke::new(if selected { 2.5_f32 } else { 1.2 }, if selected { pal.accent } else { pal.edge });
            let (pa, pb) = (self.to_screen(rect, a.pos), self.to_screen(rect, b.pos));
            if e.source == e.target {
                painter.circle_stroke(pa + Vec2::new(node_r, -node_r), node_r * 0.9, stroke);
                continue;
            }
            let dir = (pb - pa).normalized();
            let (start, end) = (pa + dir * node_r, pb - dir * (node_r + 2.0));
            painter.line_segment([start, end], stroke);
            if e.directed {
                let n = Vec2::new(-dir.y, dir.x);
                let tip = end;
                let back = tip - dir * 8.0;
                painter.add(Shape::convex_polygon(
                    vec![tip, back + n * 4.0, back - n * 4.0],
                    stroke.color,
                    Stroke::NONE,
                ));
            }
            // Labels on every edge of a dense graph are unreadable; show them when zoomed in.
            let legible = self.edges.len() <= 40 && self.zoom > 0.6 || self.zoom > 1.3;
            if selected || self.show_edge_labels && legible {
                let mid = pa + (pb - pa) * 0.5;
                painter.text(mid, Align2::CENTER_CENTER, &e.edge_type, FontId::proportional(9.5), pal.muted);
            }
        }

        // Nodes.
        let hover = response.hover_pos().and_then(|p| self.hit_node(rect, p));
        for id in &self.order {
            let v = &self.nodes[id];
            let p = self.to_screen(rect, v.pos);
            if !rect.expand(40.0).contains(p) {
                continue;
            }
            let selected = matches!(&self.selected, Some(Selection::Node(s)) if s == id);
            // 2px surface ring separates overlapping nodes.
            painter.circle_filled(p, node_r + 2.0, pal.surface);
            painter.circle_filled(p, node_r, color_of(&v.node));
            if selected || hover.as_deref() == Some(id) {
                painter.circle_stroke(p, node_r + 3.5, Stroke::new(2.0_f32, pal.accent));
            }
            if v.pinned {
                painter.circle_filled(p + Vec2::new(node_r * 0.7, -node_r * 0.7), 2.5, pal.ink);
            }
            if self.zoom > 0.35 || selected {
                painter.text(
                    p + Vec2::new(0.0, node_r + 3.0),
                    Align2::CENTER_TOP,
                    caption(&v.node),
                    font.clone(),
                    pal.ink2,
                );
            }
        }

        if self.nodes.is_empty() {
            painter.text(
                rect.center(),
                Align2::CENTER_CENTER,
                "Run a query, click a label in the Explorer, or create a node to see the graph here.",
                FontId::proportional(14.0),
                pal.muted,
            );
        }

        // Tooltip and context menu.
        if let Some(id) = &hover {
            let node = &self.nodes[id].node;
            let mut text = format!("{}\n{}", node.id, node.labels.iter().map(|l| format!(":{l}")).collect::<String>());
            for (k, val) in node.properties.iter().take(6) {
                text.push_str(&format!("\n{k}: {val}"));
            }
            response.clone().on_hover_text_at_pointer(text);
        }
        let menu_target = response.interact_pointer_pos().and_then(|p| self.hit_node(rect, p));
        response.context_menu(|ui| {
            let target = menu_target.clone().or_else(|| self.selected_node().map(|n| n.id.clone()));
            match target {
                Some(id) => {
                    ui.label(egui::RichText::new(&id).strong());
                    ui.separator();
                    if ui.button("Expand neighbors").clicked() {
                        actions.push(CanvasAction::Expand(id.clone()));
                        ui.close();
                    }
                    if ui.button("Connect from here…").clicked() {
                        actions.push(CanvasAction::ConnectFrom(id.clone()));
                        ui.close();
                    }
                    let pinned = self.nodes.get(&id).is_some_and(|v| v.pinned);
                    if ui.button(if pinned { "Unpin" } else { "Pin position" }).clicked() {
                        if let Some(v) = self.nodes.get_mut(&id) {
                            v.pinned = !pinned;
                        }
                        ui.close();
                    }
                    if ui.button("Hide from view").clicked() {
                        self.remove_node(&id);
                        ui.close();
                    }
                    ui.separator();
                    if ui.button(egui::RichText::new("Delete node…").color(pal.danger)).clicked() {
                        actions.push(CanvasAction::DeleteNode(id.clone()));
                        ui.close();
                    }
                }
                None => {
                    if ui.button("Fit to window").clicked() {
                        self.fit(rect);
                        ui.close();
                    }
                    if ui.button("Re-run layout").clicked() {
                        self.relayout();
                        ui.close();
                    }
                }
            }
        });

        // Legend (labels present in view), bottom-left.
        let mut seen: Vec<&String> = Vec::new();
        for v in self.nodes.values() {
            if let Some(l) = v.node.labels.iter().next()
                && !seen.contains(&l)
            {
                seen.push(l);
            }
        }
        seen.sort();
        let mut y = rect.bottom() - 14.0 - 16.0 * seen.len().min(10) as f32;
        for l in seen.into_iter().take(10) {
            let c = pal.label_color(label_slots.get(l));
            painter.circle_filled(Pos2::new(rect.left() + 16.0, y + 6.0), 5.0, c);
            painter.text(
                Pos2::new(rect.left() + 26.0, y),
                Align2::LEFT_TOP,
                format!(":{l}"),
                FontId::proportional(11.0),
                pal.ink2,
            );
            y += 16.0;
        }
        painter.text(
            rect.right_bottom() + Vec2::new(-10.0, -8.0),
            Align2::RIGHT_BOTTOM,
            "drag to move · scroll to zoom · double-click to expand · right-click for actions",
            FontId::proportional(10.5),
            pal.muted,
        );
        painter.rect_stroke(rect, CornerRadius::ZERO, Stroke::new(1.0_f32, pal.border), StrokeKind::Inside);
        actions
    }

    fn handle_input(&mut self, ui: &Ui, response: &Response, rect: Rect, actions: &mut Vec<CanvasAction>) {
        // Zoom around the cursor.
        if response.hovered() {
            let scroll = ui.ctx().input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                self.auto_fit = false;
            }
            if scroll != 0.0
                && let Some(p) = response.hover_pos()
            {
                let before = self.to_world(rect, p);
                self.zoom = (self.zoom * (scroll * 0.0015).exp()).clamp(0.05, 5.0);
                let after = self.to_screen(rect, before);
                self.pan += p - after;
            }
        }
        if response.drag_started() {
            self.dragging = response.interact_pointer_pos().and_then(|p| self.hit_node(rect, p));
        }
        if response.dragged() {
            let delta = response.drag_delta();
            match &self.dragging {
                Some(id) => {
                    let z = self.zoom;
                    if let Some(v) = self.nodes.get_mut(id) {
                        v.pos += delta / z;
                        v.pinned = true;
                    }
                    self.temperature = self.temperature.max(0.3);
                }
                None => {
                    self.auto_fit = false;
                    self.pan += delta;
                }
            }
        }
        if response.drag_stopped() {
            self.dragging = None;
        }
        if response.double_clicked() {
            if let Some(id) = response.interact_pointer_pos().and_then(|p| self.hit_node(rect, p)) {
                actions.push(CanvasAction::Expand(id));
            }
        } else if response.clicked() {
            let p = response.interact_pointer_pos();
            let sel = p.and_then(|p| {
                self.hit_node(rect, p).map(Selection::Node).or_else(|| self.hit_edge(rect, p).map(Selection::Edge))
            });
            self.selected = sel;
            actions.push(CanvasAction::Select);
        }
    }
}
