//! Pure 2D canvas layout primitive.
//!
//! Unlike [`ScrollingSpace`](super::scrolling::ScrollingSpace), a [`CanvasSpace`] has no column
//! structure: every tile has an independent `(canvas_x, canvas_y)` and the camera pans on both
//! axes. This is the home of the "2D infinite canvas" layout. Integration into
//! [`Workspace`](super::workspace::Workspace) is deliberately deferred — this module is built
//! bottom-up so each layer can be tested in isolation.

use std::rc::Rc;

use niri_ipc::SizeChange;
use smithay::backend::renderer::element::utils::{
    Relocate, RelocateRenderElement, RescaleRenderElement,
};
use smithay::backend::renderer::gles::GlesRenderer;
use smithay::utils::{Logical, Point, Rectangle, Scale, Serial, Size};

use super::closing_window::{ClosingWindow, ClosingWindowRenderElement};
use super::floating::DIRECTIONAL_MOVE_PX;
use super::scrolling::SpatialDirection;
use super::tile::{Tile, TileRenderElement, TileRenderSnapshot};
use super::workspace::InteractiveResize;
use super::{Canvas, HitType, InteractiveResizeData, LayoutElement, Options};
use crate::animation::{Animation, Clock};
use crate::niri_render_elements;
use crate::render_helpers::renderer::NiriRenderer;
use crate::render_helpers::xray::XrayPos;
use crate::render_helpers::RenderCtx;
use crate::utils::transaction::TransactionBlocker;
use crate::utils::{ensure_min_max_size, ResizeEdge};

niri_render_elements! {
    CanvasSpaceRenderElement<R> => {
        Tile = TileRenderElement<R>,
        TileScaled = RelocateRenderElement<RescaleRenderElement<TileRenderElement<R>>>,
        ClosingWindow = ClosingWindowRenderElement,
    }
}

/// Per-render/hit-test override that replaces the canvas camera with a synthetic view that fits
/// the populated content bounding box into the workspace view rect.
///
/// Used only for overview rendering and hit-testing. Does not mutate [`CanvasSpace`] state.
#[derive(Debug, Clone, Copy)]
pub struct OverviewFit {
    /// Synthetic camera position. `canvas_pos - view_pos` gives the un-scaled screen offset.
    pub view_pos: Point<f64, Canvas>,
    /// Scale factor applied to tile positions and visuals so the content bbox fits into the view.
    pub scale: f64,
}

/// A 2D canvas populated by free-placement tiles.
#[derive(Debug)]
pub struct CanvasSpace<W: LayoutElement> {
    /// Tiles on this canvas. Order is creation order — not z-order, not spatial.
    ///
    /// Per-tile canvas position lives on `Tile::canvas_pos` so rendering and spatial logic can
    /// read it without going through this space.
    tiles: Vec<Tile<W>>,

    /// Id of the active tile, if any. Always set to `Some` when `tiles` is non-empty.
    active_id: Option<W::Id>,

    /// Horizontal camera position (absolute canvas-space X of the viewport's left edge).
    view_offset_x: AxisCamera,

    /// Vertical camera position (absolute canvas-space Y of the viewport's top edge).
    view_offset_y: AxisCamera,

    /// View size for this space.
    view_size: Size<f64, Logical>,

    /// Working area (view minus layer-shell struts).
    working_area: Rectangle<f64, Logical>,

    /// Output scale for physical-pixel rounding.
    scale: f64,

    /// Tiles whose source window has gone away but whose close animation is still running.
    closing_windows: Vec<ClosingWindow>,

    /// Active interactive resize for a canvas tile, if any.
    interactive_resize: Option<InteractiveResize<W>>,

    /// Clock for driving animations.
    clock: Clock,

    /// Configurable properties.
    options: Rc<Options>,
}

/// Single-axis camera — static value or spring animation.
#[derive(Debug)]
enum AxisCamera {
    Static(f64),
    Animation(Animation),
}

impl AxisCamera {
    fn current(&self) -> f64 {
        match self {
            AxisCamera::Static(v) => *v,
            AxisCamera::Animation(a) => a.value(),
        }
    }

    fn target(&self) -> f64 {
        match self {
            AxisCamera::Static(v) => *v,
            AxisCamera::Animation(a) => a.to(),
        }
    }

    fn is_static(&self) -> bool {
        matches!(self, AxisCamera::Static(_))
    }

    fn is_animation_ongoing(&self) -> bool {
        matches!(self, AxisCamera::Animation(_))
    }
}

impl<W: LayoutElement> CanvasSpace<W> {
    pub fn new(
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        clock: Clock,
        options: Rc<Options>,
    ) -> Self {
        Self {
            tiles: Vec::new(),
            active_id: None,
            view_offset_x: AxisCamera::Static(0.),
            view_offset_y: AxisCamera::Static(0.),
            view_size,
            working_area,
            scale,
            closing_windows: Vec::new(),
            interactive_resize: None,
            clock,
            options,
        }
    }

    pub fn update_config(
        &mut self,
        view_size: Size<f64, Logical>,
        working_area: Rectangle<f64, Logical>,
        scale: f64,
        options: Rc<Options>,
    ) {
        for tile in &mut self.tiles {
            tile.update_config(view_size, scale, options.clone());
        }
        self.view_size = view_size;
        self.working_area = working_area;
        self.scale = scale;
        self.options = options;
    }

    pub fn is_empty(&self) -> bool {
        self.tiles.is_empty()
    }

    pub fn len(&self) -> usize {
        self.tiles.len()
    }

    /// Append `tile` to the canvas at `canvas_pos`, activating it.
    pub fn add_tile(&mut self, mut tile: Tile<W>, canvas_pos: Point<f64, Canvas>) {
        tile.set_canvas_pos(canvas_pos);
        self.active_id = Some(tile.window().id().clone());
        self.tiles.push(tile);
    }

    /// Remove the tile with the given id. Returns the tile on success.
    pub fn remove_tile(&mut self, id: &W::Id) -> Option<Tile<W>> {
        let idx = self.tiles.iter().position(|t| t.window().id() == id)?;
        let tile = self.tiles.remove(idx);

        // Fix up the active id if we just removed it.
        if self.active_id.as_ref() == Some(id) {
            self.active_id = self.tiles.first().map(|t| t.window().id().clone());
        }
        Some(tile)
    }

    pub fn tiles(&self) -> impl Iterator<Item = &Tile<W>> + '_ {
        self.tiles.iter()
    }

    pub fn tiles_mut(&mut self) -> impl Iterator<Item = &mut Tile<W>> + '_ {
        self.tiles.iter_mut()
    }

    pub fn active_window(&self) -> Option<&W> {
        let id = self.active_id.as_ref()?;
        self.tiles
            .iter()
            .find(|t| t.window().id() == id)
            .map(Tile::window)
    }

    pub fn active_window_mut(&mut self) -> Option<&mut W> {
        let id = self.active_id.clone()?;
        self.tiles
            .iter_mut()
            .find(|t| t.window().id() == &id)
            .map(Tile::window_mut)
    }

    pub fn active_window_id(&self) -> Option<&W::Id> {
        self.active_id.as_ref()
    }

    /// Activate the tile with the given id. Returns true if the id matched a tile.
    pub fn activate_window(&mut self, id: &W::Id) -> bool {
        if self.tiles.iter().any(|t| t.window().id() == id) {
            self.active_id = Some(id.clone());
            true
        } else {
            false
        }
    }

    /// Move the tile identified by `id` to the given canvas position. No-op if not found.
    pub fn move_tile_to(&mut self, id: &W::Id, canvas_pos: Point<f64, Canvas>) -> bool {
        let Some(tile) = self.tiles.iter_mut().find(|t| t.window().id() == id) else {
            return false;
        };
        tile.set_canvas_pos(canvas_pos);
        true
    }

    /// Mirror of other spaces: does this canvas contain the window id?
    pub fn has_window(&self, id: &W::Id) -> bool {
        self.tiles.iter().any(|t| t.window().id() == id)
    }

    /// Alias of [`has_window`] matching [`ScrollingSpace::contains`].
    pub fn contains(&self, id: &W::Id) -> bool {
        self.has_window(id)
    }

    /// Activate a tile without any z-ordering side effects. Returns true if the id matched.
    ///
    /// CanvasSpace has no z-order in this phase, so this is equivalent to [`activate_window`],
    /// but the distinct name mirrors [`FloatingSpace`] and lets Workspace use a uniform API.
    pub fn activate_window_without_raising(&mut self, id: &W::Id) -> bool {
        self.activate_window(id)
    }

    /// Propagate the window's latest state into its tile (size changes, sizing mode, etc.).
    ///
    /// Mirrors [`ScrollingSpace::update_window`] but without column resizing: in a canvas,
    /// each tile lives on its own canvas_pos, so resizing doesn't ripple to neighbors.
    pub fn update_window(&mut self, id: &W::Id, serial: Option<Serial>) -> bool {
        let Some(tile) = self.tiles.iter_mut().find(|t| t.window().id() == id) else {
            return false;
        };
        if let Some(serial) = serial {
            tile.window_mut().on_commit(serial);
        }
        tile.update_window();
        true
    }

    /// Emit render elements for every tile at its camera-offset screen-space position.
    ///
    /// Mirrors the shape of [`FloatingSpace::render`] / [`ScrollingSpace::render`]: the caller
    /// supplies a push callback that receives a [`CanvasSpaceRenderElement`] per element.
    ///
    /// When `overview` is `Some`, the canvas renders via a synthetic view — see
    /// [`OverviewFit`]. This is used to fit the populated content into an overview thumbnail.
    pub fn render<R: NiriRenderer>(
        &self,
        mut ctx: RenderCtx<R>,
        xray_pos: XrayPos,
        focus_ring: bool,
        overview: Option<OverviewFit>,
        push: &mut dyn FnMut(CanvasSpaceRenderElement<R>),
    ) {
        let scale = Scale::from(self.scale);

        // Draw closing-window animations on top of tiles so a tile that's mid-close doesn't get
        // occluded by unrelated canvas tiles that happen to be in front in insertion order.
        // The view_rect for canvas is always anchored at (0, 0) in screen-space — the camera
        // offset is baked into ClosingWindow::pos when the animation starts.
        //
        // Skip in overview fit mode: closing windows have baked-in screen positions that don't
        // respect the overview override and would render at the wrong place. Overview is a
        // short-lived view so this is a non-issue in practice.
        if overview.is_none() {
            let view_rect = Rectangle::from_size(self.view_size);
            for closing in self.closing_windows.iter().rev() {
                let elem = closing.render(ctx.as_gles(), view_rect, scale);
                push(elem.into());
            }
        }

        let active = self.active_id.clone();
        match overview {
            None => {
                for (tile, tile_pos) in self.tiles_with_render_positions() {
                    let focus_ring = focus_ring && Some(tile.window().id()) == active.as_ref();
                    let xray_pos = xray_pos.offset(tile_pos);
                    tile.render(ctx.r(), tile_pos, xray_pos, focus_ring, &mut |elem| {
                        push(elem.into())
                    });
                }
            }
            Some(fit) => {
                for tile in &self.tiles {
                    let focus_ring = focus_ring && Some(tile.window().id()) == active.as_ref();
                    let base = Self::canvas_to_screen_base(tile.canvas_pos(), fit.view_pos)
                        + tile.render_offset();
                    let tile_pos =
                        Point::<f64, Logical>::from((base.x * fit.scale, base.y * fit.scale));
                    let tile_pos = tile_pos
                        .to_physical_precise_round(self.scale)
                        .to_logical(self.scale);
                    let xray_pos = xray_pos.offset(tile_pos);
                    let anchor = tile_pos.to_physical_precise_round(self.scale);
                    // Tile renders at tile_pos; wrap elements to shrink them around that anchor.
                    tile.render(ctx.r(), tile_pos, xray_pos, focus_ring, &mut |elem| {
                        let elem = RescaleRenderElement::from_element(elem, anchor, fit.scale);
                        let elem = RelocateRenderElement::from_element(
                            elem,
                            Point::default(),
                            Relocate::Relative,
                        );
                        push(elem.into());
                    });
                }
            }
        }
    }

    pub fn interactive_resize_begin(&mut self, window: W::Id, edges: ResizeEdge) -> bool {
        if self.interactive_resize.is_some() {
            return false;
        }

        let Some(tile) = self.tiles.iter_mut().find(|t| t.window().id() == &window) else {
            return false;
        };

        let original_window_size = tile.window_size();

        self.interactive_resize = Some(InteractiveResize {
            window,
            original_window_size,
            data: InteractiveResizeData { edges },
        });

        // Stop camera animations so the tile doesn't slide out from under the cursor.
        self.view_offset_x = AxisCamera::Static(self.view_offset_x.current());
        self.view_offset_y = AxisCamera::Static(self.view_offset_y.current());

        true
    }

    pub fn interactive_resize_update(
        &mut self,
        window: &W::Id,
        delta: Point<f64, Logical>,
    ) -> bool {
        let Some(resize) = &self.interactive_resize else {
            return false;
        };
        if window != &resize.window {
            return false;
        }

        let original = resize.original_window_size;
        let edges = resize.data.edges;

        if edges.intersects(ResizeEdge::LEFT_RIGHT) {
            let mut dx = delta.x;
            if edges.contains(ResizeEdge::LEFT) {
                dx = -dx;
            }
            let win_width = (original.w + dx).round() as i32;
            self.set_window_width(Some(window), SizeChange::SetFixed(win_width), false);
        }

        if edges.intersects(ResizeEdge::TOP_BOTTOM) {
            let mut dy = delta.y;
            if edges.contains(ResizeEdge::TOP) {
                dy = -dy;
            }
            let win_height = (original.h + dy).round() as i32;
            self.set_window_height(Some(window), SizeChange::SetFixed(win_height), false);
        }

        true
    }

    pub fn interactive_resize_end(&mut self, window: Option<&W::Id>) {
        let Some(resize) = &self.interactive_resize else {
            return;
        };
        if let Some(window) = window {
            if window != &resize.window {
                return;
            }
        }
        self.interactive_resize = None;
    }

    /// Set the window width for the identified tile. Mirrors `FloatingSpace::set_window_width`:
    /// canvas tiles are laid out independently, so there's no column-width distribution to do.
    pub fn set_window_width(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(id) = id.or(self.active_id.as_ref()) else {
            return;
        };
        let Some(tile) = self.tiles.iter_mut().find(|t| t.window().id() == id) else {
            return;
        };

        let available_size = self.working_area.size.w;
        let win = tile.window();
        let current_window = win.expected_size().unwrap_or_else(|| win.size()).w;
        let current_tile = tile.tile_expected_or_current_size().w;

        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let win_width = match change {
            SizeChange::SetFixed(w) => f64::from(w),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                let tile_width = available_size * prop;
                tile.window_width_for_tile_width(tile_width)
            }
            SizeChange::AdjustFixed(delta) => f64::from(current_window.saturating_add(delta)),
            SizeChange::AdjustProportion(delta) => {
                let current_prop = current_tile / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., MAX_F);
                let tile_width = available_size * prop;
                tile.window_width_for_tile_width(tile_width)
            }
        };
        let win_width = win_width.round().clamp(1., MAX_PX) as i32;

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();

        let win_width = ensure_min_max_size(win_width, min_size.w, max_size.w);
        let win_height = win.expected_size().unwrap_or_default().h;
        let win_height = ensure_min_max_size(win_height, min_size.h, max_size.h);

        win.request_size_once(Size::from((win_width, win_height)), animate);
    }

    pub fn set_window_height(&mut self, id: Option<&W::Id>, change: SizeChange, animate: bool) {
        let Some(id) = id.or(self.active_id.as_ref()) else {
            return;
        };
        let Some(tile) = self.tiles.iter_mut().find(|t| t.window().id() == id) else {
            return;
        };

        let available_size = self.working_area.size.h;
        let win = tile.window();
        let current_window = win.expected_size().unwrap_or_else(|| win.size()).h;
        let current_tile = tile.tile_expected_or_current_size().h;

        const MAX_PX: f64 = 100000.;
        const MAX_F: f64 = 10000.;

        let win_height = match change {
            SizeChange::SetFixed(h) => f64::from(h),
            SizeChange::SetProportion(prop) => {
                let prop = (prop / 100.).clamp(0., MAX_F);
                let tile_height = available_size * prop;
                tile.window_height_for_tile_height(tile_height)
            }
            SizeChange::AdjustFixed(delta) => f64::from(current_window.saturating_add(delta)),
            SizeChange::AdjustProportion(delta) => {
                let current_prop = current_tile / available_size;
                let prop = (current_prop + delta / 100.).clamp(0., MAX_F);
                let tile_height = available_size * prop;
                tile.window_height_for_tile_height(tile_height)
            }
        };
        let win_height = win_height.round().clamp(1., MAX_PX) as i32;

        let win = tile.window_mut();
        let min_size = win.min_size();
        let max_size = win.max_size();

        let win_height = ensure_min_max_size(win_height, min_size.h, max_size.h);
        let win_width = win.expected_size().unwrap_or_default().w;
        let win_width = ensure_min_max_size(win_width, min_size.w, max_size.w);

        win.request_size_once(Size::from((win_width, win_height)), animate);
    }

    /// Start a close animation for a tile still present on this canvas.
    ///
    /// Snapshots the tile's current render contents, then hands off to
    /// [`start_close_animation_for_tile`]. The caller removes the tile separately.
    pub fn start_close_animation_for_window(
        &mut self,
        renderer: &mut GlesRenderer,
        id: &W::Id,
        blocker: TransactionBlocker,
    ) {
        let (tile, tile_pos) = match self
            .tiles_with_render_positions_mut(false)
            .find(|(tile, _)| tile.window().id() == id)
        {
            Some(rv) => rv,
            None => return,
        };

        let Some(snapshot) = tile.take_unmap_snapshot() else {
            return;
        };

        let tile_size = tile.tile_size();

        self.start_close_animation_for_tile(renderer, snapshot, tile_size, tile_pos, blocker);
    }

    /// Drive a close animation from a pre-captured snapshot at the given screen-space position.
    pub fn start_close_animation_for_tile(
        &mut self,
        renderer: &mut GlesRenderer,
        snapshot: TileRenderSnapshot,
        tile_size: Size<f64, Logical>,
        tile_pos: Point<f64, Logical>,
        blocker: TransactionBlocker,
    ) {
        let anim = Animation::new(
            self.clock.clone(),
            0.,
            1.,
            0.,
            self.options.animations.window_close.anim,
        );

        let blocker = if self.options.disable_transactions {
            TransactionBlocker::completed()
        } else {
            blocker
        };

        let scale = Scale::from(self.scale);
        let res = ClosingWindow::new(
            renderer, snapshot, scale, tile_size, tile_pos, blocker, anim,
        );
        match res {
            Ok(closing) => {
                self.closing_windows.push(closing);
            }
            Err(err) => {
                tracing::warn!("error creating a closing window animation: {err:?}");
            }
        }
    }

    /// Hit-test the canvas at the given logical screen-space point.
    ///
    /// Iterates tiles in reverse insertion order so later-added tiles are tried first (stands in
    /// for proper z-order in this phase).
    ///
    /// When `overview` is `Some`, the same fit transform used in [`render`] is applied: tile
    /// positions are scaled and the hit box matches the visually rendered rect.
    pub fn window_under(
        &self,
        point: Point<f64, Logical>,
        overview: Option<OverviewFit>,
    ) -> Option<(&W, HitType)> {
        let scale = self.scale;
        match overview {
            None => {
                let view_pos = self.view_pos();
                for tile in self.tiles.iter().rev() {
                    let tile_pos = Self::canvas_to_screen_base(tile.canvas_pos(), view_pos)
                        + tile.render_offset();
                    let tile_pos = tile_pos.to_physical_precise_round(scale).to_logical(scale);

                    if let Some(rv) = HitType::hit_tile(tile, tile_pos, point) {
                        return Some(rv);
                    }
                }
            }
            Some(fit) => {
                // Hit-test in the un-scaled synthetic screen space. Divide the hit point by
                // fit.scale, hit-test against un-scaled tile rects anchored at
                // (canvas_pos - fit.view_pos) + render_offset.
                let point_unscaled =
                    Point::<f64, Logical>::from((point.x / fit.scale, point.y / fit.scale));
                for tile in self.tiles.iter().rev() {
                    let tile_pos = Self::canvas_to_screen_base(tile.canvas_pos(), fit.view_pos)
                        + tile.render_offset();
                    if let Some(rv) = HitType::hit_tile(tile, tile_pos, point_unscaled) {
                        return Some(rv);
                    }
                }
            }
        }
        None
    }

    /// Compute the canvas-space bounding box of all tiles. Returns `None` if empty.
    pub fn content_bounds(&self) -> Option<Rectangle<f64, Canvas>> {
        let mut iter = self.tiles.iter();
        let first = iter.next()?;
        let first_pos = first.canvas_pos();
        let first_size = first.tile_size();
        let mut min_x = first_pos.x;
        let mut min_y = first_pos.y;
        let mut max_x = first_pos.x + first_size.w;
        let mut max_y = first_pos.y + first_size.h;
        for tile in iter {
            let pos = tile.canvas_pos();
            let size = tile.tile_size();
            if pos.x < min_x {
                min_x = pos.x;
            }
            if pos.y < min_y {
                min_y = pos.y;
            }
            if pos.x + size.w > max_x {
                max_x = pos.x + size.w;
            }
            if pos.y + size.h > max_y {
                max_y = pos.y + size.h;
            }
        }
        Some(Rectangle::new(
            Point::<f64, Canvas>::from((min_x, min_y)),
            Size::<f64, Canvas>::from((max_x - min_x, max_y - min_y)),
        ))
    }

    /// Compute the overview-fit transform that centers the populated content in `view_size`.
    ///
    /// Returns `None` when the canvas is empty, when `view_size` is degenerate, or when the
    /// content already fits at scale 1 (no fit needed). The caller may still use `Some(fit)` with
    /// `scale == 1.0` when extra padding centering is desired; the threshold here is a modest
    /// margin beyond the raw bbox.
    pub fn overview_fit(&self) -> Option<OverviewFit> {
        let bbox = self.content_bounds()?;
        if !(self.view_size.w > 0. && self.view_size.h > 0.) {
            return None;
        }

        // Padding in logical pixels so tiles don't render flush with the workspace edge.
        let padding: f64 = 32.;
        let padded_w = (bbox.size.w + padding * 2.).max(1e-3);
        let padded_h = (bbox.size.h + padding * 2.).max(1e-3);

        let fit_x = self.view_size.w / padded_w;
        let fit_y = self.view_size.h / padded_h;
        let scale = fit_x.min(fit_y).clamp(1e-3, 1.0);

        let bbox_center_x = bbox.loc.x + bbox.size.w / 2.;
        let bbox_center_y = bbox.loc.y + bbox.size.h / 2.;
        let view_pos = Point::<f64, Canvas>::from((
            bbox_center_x - self.view_size.w / (2. * scale),
            bbox_center_y - self.view_size.h / (2. * scale),
        ));
        Some(OverviewFit { view_pos, scale })
    }

    /// Debug-mode invariants for `Layout::verify_invariants`.
    ///
    /// Must hold at all times: scale is positive, tiles agree with the space's config, the
    /// active id (if any) references an existing tile, and tile ids are unique.
    #[cfg(test)]
    pub fn verify_invariants(&self) {
        assert!(self.scale > 0.);
        assert!(self.scale.is_finite());

        for tile in &self.tiles {
            assert!(Rc::ptr_eq(&self.options, tile.options()));
            assert_eq!(self.view_size, tile.view_size());
            assert_eq!(self.scale, tile.scale());
            tile.verify_invariants();
        }

        if let Some(id) = &self.active_id {
            assert!(
                self.tiles.iter().any(|t| t.window().id() == id),
                "active_id must reference an existing tile",
            );
        }

        // Tile ids must be unique.
        for (i, a) in self.tiles.iter().enumerate() {
            for b in self.tiles.iter().skip(i + 1) {
                assert!(
                    a.window().id() != b.window().id(),
                    "duplicate tile id on canvas",
                );
            }
        }
    }

    /// Iterate over tiles with their canonical canvas positions (stable under camera / anim).
    pub fn tiles_with_canvas_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Canvas>)> + '_ {
        self.tiles.iter().map(|tile| (tile, tile.canvas_pos()))
    }

    /// Iterate over tiles with screen-space positions: canvas_pos minus camera, then rounded.
    pub fn tiles_with_render_positions(
        &self,
    ) -> impl Iterator<Item = (&Tile<W>, Point<f64, Logical>)> + '_ {
        let view_pos = self.view_pos();
        let scale = self.scale;
        self.tiles.iter().map(move |tile| {
            let pos =
                Self::canvas_to_screen_base(tile.canvas_pos(), view_pos) + tile.render_offset();
            let pos = pos.to_physical_precise_round(scale).to_logical(scale);
            (tile, pos)
        })
    }

    /// Mutable variant of [`tiles_with_render_positions`].
    ///
    /// `round` controls whether the returned screen-space point is snapped to physical pixels —
    /// callers that already round separately pass `false` to avoid double-rounding.
    pub fn tiles_with_render_positions_mut(
        &mut self,
        round: bool,
    ) -> impl Iterator<Item = (&mut Tile<W>, Point<f64, Logical>)> + '_ {
        let view_pos = self.view_pos();
        let scale = self.scale;
        self.tiles.iter_mut().map(move |tile| {
            let pos =
                Self::canvas_to_screen_base(tile.canvas_pos(), view_pos) + tile.render_offset();
            let pos = if round {
                pos.to_physical_precise_round(scale).to_logical(scale)
            } else {
                pos
            };
            (tile, pos)
        })
    }

    /// Transform a canvas-space point into static screen-space (no per-tile animation offsets).
    pub(super) fn canvas_to_screen_base(
        canvas: Point<f64, Canvas>,
        view_pos: Point<f64, Canvas>,
    ) -> Point<f64, Logical> {
        Point::<f64, Logical>::from((canvas.x - view_pos.x, canvas.y - view_pos.y))
    }

    // --- camera ---

    pub fn view_pos(&self) -> Point<f64, Canvas> {
        Point::from((self.view_offset_x.current(), self.view_offset_y.current()))
    }

    pub fn target_view_pos(&self) -> Point<f64, Canvas> {
        Point::from((self.view_offset_x.target(), self.view_offset_y.target()))
    }

    pub fn view_pos_x(&self) -> f64 {
        self.view_offset_x.current()
    }

    pub fn view_pos_y(&self) -> f64 {
        self.view_offset_y.current()
    }

    pub fn target_view_pos_x(&self) -> f64 {
        self.view_offset_x.target()
    }

    pub fn target_view_pos_y(&self) -> f64 {
        self.view_offset_y.target()
    }

    /// Jumps the camera to an absolute canvas position without animation.
    pub fn set_view_pos(&mut self, pos: Point<f64, Canvas>) {
        self.view_offset_x = AxisCamera::Static(pos.x);
        self.view_offset_y = AxisCamera::Static(pos.y);
    }

    /// Animates the X camera toward `new_x` using the horizontal-view-movement spring.
    pub fn animate_view_pos_x(&mut self, new_x: f64) {
        let config = self.options.animations.horizontal_view_movement.0;
        if self.view_offset_x.target() == new_x {
            return;
        }
        self.view_offset_x = AxisCamera::Animation(Animation::new(
            self.clock.clone(),
            self.view_offset_x.current(),
            new_x,
            0.,
            config,
        ));
    }

    /// Animates the Y camera toward `new_y`.
    pub fn animate_view_pos_y(&mut self, new_y: f64) {
        let config = self.options.animations.horizontal_view_movement.0;
        if self.view_offset_y.target() == new_y {
            return;
        }
        self.view_offset_y = AxisCamera::Animation(Animation::new(
            self.clock.clone(),
            self.view_offset_y.current(),
            new_y,
            0.,
            config,
        ));
    }

    /// Pan the camera by `(dx, dy)` with a spring animation.
    pub fn pan_camera(&mut self, dx: f64, dy: f64) {
        if dx != 0. {
            self.animate_view_pos_x(self.view_offset_x.target() + dx);
        }
        if dy != 0. {
            self.animate_view_pos_y(self.view_offset_y.target() + dy);
        }
    }

    /// Fits the active tile into view on both axes (only pans where needed).
    pub fn bring_active_tile_into_view(&mut self) {
        let Some(id) = self.active_id.clone() else {
            return;
        };
        let Some(tile) = self.tiles.iter().find(|t| t.window().id() == &id) else {
            return;
        };

        let canvas = tile.canvas_pos();
        let size = tile.tile_size();

        let view_left = self.target_view_pos_x();
        let view_top = self.target_view_pos_y();
        let view_right = view_left + self.view_size.w;
        let view_bottom = view_top + self.view_size.h;

        if canvas.x < view_left {
            self.animate_view_pos_x(canvas.x);
        } else if canvas.x + size.w > view_right {
            self.animate_view_pos_x(canvas.x + size.w - self.view_size.w);
        }

        if canvas.y < view_top {
            self.animate_view_pos_y(canvas.y);
        } else if canvas.y + size.h > view_bottom {
            self.animate_view_pos_y(canvas.y + size.h - self.view_size.h);
        }
    }

    // --- directional move ---

    /// Nudge the active tile by `amount` in canvas space. Returns false if there is no active
    /// tile. Brings the tile into view afterward and cancels any in-flight resize gesture, since
    /// the captured original geometry no longer applies.
    pub fn move_active_by(&mut self, amount: Point<f64, Canvas>) -> bool {
        let Some(id) = self.active_id.clone() else {
            return false;
        };
        let Some(tile) = self.tiles.iter_mut().find(|t| t.window().id() == &id) else {
            return false;
        };
        let current = tile.canvas_pos();
        tile.set_canvas_pos(Point::<f64, Canvas>::from((
            current.x + amount.x,
            current.y + amount.y,
        )));
        self.interactive_resize = None;
        self.bring_active_tile_into_view();
        true
    }

    pub fn move_active_left(&mut self) -> bool {
        self.move_active_by(Point::from((-DIRECTIONAL_MOVE_PX, 0.)))
    }

    pub fn move_active_right(&mut self) -> bool {
        self.move_active_by(Point::from((DIRECTIONAL_MOVE_PX, 0.)))
    }

    pub fn move_active_up(&mut self) -> bool {
        self.move_active_by(Point::from((0., -DIRECTIONAL_MOVE_PX)))
    }

    pub fn move_active_down(&mut self) -> bool {
        self.move_active_by(Point::from((0., DIRECTIONAL_MOVE_PX)))
    }

    // --- spatial focus ---

    /// Activate the nearest tile in `direction`, scored in canvas space.
    ///
    /// Uses the same `primary + 2 * |perpendicular|` rule as the scrolling-space spatial focus,
    /// so cross-space navigation in Workspace remains consistent.
    pub fn focus_spatial(&mut self, direction: SpatialDirection) -> bool {
        let Some(active_id) = self.active_id.clone() else {
            return false;
        };

        let mut active_center: Option<(f64, f64)> = None;
        let mut candidates: Vec<(W::Id, f64, f64)> = Vec::new();

        for tile in &self.tiles {
            let canvas = tile.canvas_pos();
            let size = tile.tile_size();
            let cx = canvas.x + size.w / 2.;
            let cy = canvas.y + size.h / 2.;
            if tile.window().id() == &active_id {
                active_center = Some((cx, cy));
            } else {
                candidates.push((tile.window().id().clone(), cx, cy));
            }
        }

        let Some((ax, ay)) = active_center else {
            return false;
        };

        let mut best: Option<(f64, W::Id)> = None;
        for (id, cx, cy) in candidates {
            let dx = cx - ax;
            let dy = cy - ay;
            let score = match direction {
                SpatialDirection::Right if dx > 0. => dx + 2. * dy.abs(),
                SpatialDirection::Left if dx < 0. => -dx + 2. * dy.abs(),
                SpatialDirection::Down if dy > 0. => dy + 2. * dx.abs(),
                SpatialDirection::Up if dy < 0. => -dy + 2. * dx.abs(),
                _ => continue,
            };
            if best.as_ref().is_none_or(|(s, _)| score < *s) {
                best = Some((score, id));
            }
        }

        match best {
            Some((_, id)) => {
                self.active_id = Some(id);
                self.bring_active_tile_into_view();
                true
            }
            None => false,
        }
    }

    // --- animation lifecycle ---

    pub fn advance_animations(&mut self) {
        if let AxisCamera::Animation(anim) = &self.view_offset_x {
            if anim.is_done() {
                self.view_offset_x = AxisCamera::Static(anim.to());
            }
        }
        if let AxisCamera::Animation(anim) = &self.view_offset_y {
            if anim.is_done() {
                self.view_offset_y = AxisCamera::Static(anim.to());
            }
        }
        for tile in &mut self.tiles {
            tile.advance_animations();
        }
        self.closing_windows.retain_mut(|closing| {
            closing.advance_animations();
            closing.are_animations_ongoing()
        });
    }

    pub fn are_animations_ongoing(&self) -> bool {
        self.view_offset_x.is_animation_ongoing()
            || self.view_offset_y.is_animation_ongoing()
            || self.tiles.iter().any(Tile::are_animations_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn are_transitions_ongoing(&self) -> bool {
        !self.view_offset_x.is_static()
            || !self.view_offset_y.is_static()
            || self.tiles.iter().any(Tile::are_transitions_ongoing)
            || !self.closing_windows.is_empty()
    }

    pub fn update_render_elements(&mut self, is_active: bool) {
        let view_pos = self.view_pos();
        let view_size = self.view_size;
        let active_id = self.active_id.clone();
        for tile in &mut self.tiles {
            let tile_active = is_active && active_id.as_ref() == Some(tile.window().id());
            let tile_canvas = tile.canvas_pos();
            let view_rect = Rectangle::new(
                Point::<f64, Logical>::from((
                    view_pos.x - tile_canvas.x,
                    view_pos.y - tile_canvas.y,
                )),
                view_size,
            );
            tile.update_render_elements(tile_active, view_rect);
        }
    }

    /// Keep [`Tile::canvas_pos`] in sync with the space's source of truth.
    ///
    /// In CanvasSpace the tile's own `canvas_pos` IS the source of truth — this method exists
    /// only to mirror the API surface of other spaces so that Workspace can call it uniformly.
    pub fn update_canvas_positions(&mut self) {
        // No-op: tile.canvas_pos is already the canonical value here.
    }

    pub fn view_size(&self) -> Size<f64, Logical> {
        self.view_size
    }

    pub fn working_area(&self) -> Rectangle<f64, Logical> {
        self.working_area
    }

    pub fn scale(&self) -> f64 {
        self.scale
    }

    pub fn options(&self) -> &Rc<Options> {
        &self.options
    }
}
