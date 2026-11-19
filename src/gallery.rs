//! Logical grid coordinates independent of HWND size and scroll events.

use crate::carousel::Carousel;
use eframe::egui::{Rect, Vec2, pos2, vec2};
use std::ops::Range;

const GAP: f32 = 18.0;
const SIDE: f32 = 28.0;
const TOP: f32 = 36.0;
const BOTTOM: f32 = 48.0;
const TILE_RATIO: f32 = 9.0 / 16.0;
const OVERSCAN_ROWS: usize = 1;

/// One set of logical measurements sizes the native window and its grid.
/// Keep it fixed across both directions of the same transition.
pub struct Layout {
    pub size: Vec2,
    columns: usize,
    tile_width: f32,
}

impl Layout {
    pub fn new(available: Vec2, collapsed: Vec2, len: usize) -> Self {
        let chrome = vec2(SIDE * 2.0, TOP + BOTTOM);
        let space = (available.min(vec2(1560.0, 800.0)) - chrome).max(Vec2::splat(1.0));
        let columns = (((space.x + GAP) / (320.0 + GAP)) as usize)
            .clamp(1, 3)
            .min(len.max(1));
        let gaps = (columns - 1) as f32 * GAP;
        let width = ((space.x - gaps) / columns as f32).min(match columns {
            1 => 736.0,
            2 => 560.0,
            _ => space.x,
        });
        let two_row_width = (space.y - GAP) / (TILE_RATIO * 2.0);
        // Short work areas keep one complete, large row and scroll sooner.
        let two_rows = len.div_ceil(columns) > 1 && two_row_width >= width.min(300.0);
        let (rows, row_gap) = if two_rows { (2.0, GAP) } else { (1.0, 0.0) };
        let tile_width = width.min((space.y - row_gap) / (TILE_RATIO * rows));
        let content = vec2(
            tile_width * columns as f32 + gaps,
            tile_width * TILE_RATIO * rows + row_gap,
        );
        Self {
            size: (content + chrome).max(collapsed).min(available),
            columns,
            tile_width,
        }
    }

    pub fn grid(&self, outer: Rect, len: usize) -> Grid {
        let scale = (outer.width() / self.size.x).min(outer.height() / self.size.y);
        let bounds = Rect::from_min_max(
            outer.min + vec2(SIDE, TOP) * scale,
            outer.max - vec2(SIDE, BOTTOM) * scale,
        );
        Grid::new(
            bounds,
            len,
            self.columns,
            self.tile_width * scale,
            GAP * scale,
        )
    }
}

#[derive(Clone, Copy)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

pub struct Grid {
    pub columns: usize,
    bounds: Rect,
    len: usize,
    rows: usize,
    tile_width: f32,
    tile_height: f32,
    gap: f32,
    left: f32,
    top: f32,
}

impl Grid {
    fn new(bounds: Rect, len: usize, columns: usize, tile_width: f32, gap: f32) -> Self {
        let columns = columns.min(len.max(1));
        let gaps = (columns - 1) as f32 * gap;
        let total_width = columns as f32 * tile_width + gaps;
        let rows = len.div_ceil(columns);
        let tile_height = tile_width * TILE_RATIO;
        let content_height = rows as f32 * (tile_height + gap) - gap;
        Self {
            columns,
            bounds,
            len,
            rows,
            tile_width,
            tile_height,
            gap,
            left: bounds.center().x - total_width * 0.5,
            top: bounds.top() + (bounds.height() - content_height).max(0.0) * 0.5,
        }
    }

    pub fn rect(&self, index: usize, scroll: f32) -> Rect {
        let x = self.left + (index % self.columns) as f32 * (self.tile_width + self.gap);
        let y = self.top + (index / self.columns) as f32 * self.row_pitch() - scroll;
        Rect::from_min_size(pos2(x, y), vec2(self.tile_width, self.tile_height))
    }

    pub fn viewport(&self) -> Rect {
        self.bounds
    }

    pub fn clamp_scroll(&self, scroll: f32) -> f32 {
        scroll.clamp(0.0, self.max_scroll())
    }

    pub fn max_scroll(&self) -> f32 {
        let content_height = self.rows as f32 * self.row_pitch() - self.gap;
        (content_height - self.bounds.height()).max(0.0)
    }

    /// Render only the viewport and one extra row on either side. Coordinates
    /// and cache work are independent of the number of rows above the viewport.
    pub fn visible(&self, scroll: f32) -> Range<usize> {
        let scroll = self.clamp_scroll(scroll);
        let first = (scroll / self.row_pitch()) as usize;
        let end = ((scroll + self.bounds.height()) / self.row_pitch()).ceil() as usize;
        let first = first.saturating_sub(OVERSCAN_ROWS);
        let end = end.saturating_add(OVERSCAN_ROWS).min(self.rows);
        first * self.columns..end.saturating_mul(self.columns).min(self.len)
    }

    pub fn reveal(&self, index: usize, scroll: f32) -> f32 {
        let scroll = self.clamp_scroll(scroll);
        if index >= self.len {
            return scroll;
        }
        let rect = self.rect(index, 0.0);
        let top = rect.top() - self.bounds.top();
        let bottom = top + rect.height();
        self.clamp_scroll(if top < scroll || rect.height() > self.bounds.height() {
            top
        } else {
            scroll.max(bottom - self.bounds.height())
        })
    }

    pub fn move_selection(&self, index: usize, direction: Direction) -> Option<usize> {
        let index = index.min(self.len.saturating_sub(1));
        let column = index % self.columns;
        let row = index / self.columns;
        Some(match direction {
            Direction::Up if row == 0 => return None,
            Direction::Up => index - self.columns,
            Direction::Down if row + 1 < self.rows => (index + self.columns).min(self.len - 1),
            Direction::Left if column > 0 => index - 1,
            Direction::Right if column + 1 < self.columns && index + 1 < self.len => index + 1,
            _ => index,
        })
    }

    fn row_pitch(&self) -> f32 {
        self.tile_height + self.gap
    }
}

/// Use the existing critically damped spring with endpoints 0 and 1 so window
/// and image geometry share the same clock and preserve velocity on reversal.
pub struct Transition(Carousel);

impl Default for Transition {
    fn default() -> Self {
        Self(Carousel::new(0))
    }
}

impl Transition {
    pub fn progress(&self) -> f32 {
        self.0.position as f32
    }

    pub fn expanded(&self) -> bool {
        self.0.target == 1
    }

    pub fn active(&self) -> bool {
        !self.0.is_settled()
    }

    pub fn target(&mut self, expanded: bool, reduced_motion: bool) {
        self.0.select(usize::from(expanded), 2);
        if reduced_motion {
            self.0.advance(0.0, true);
        }
    }

    pub fn advance(&mut self, dt: f64, reduced_motion: bool) -> bool {
        self.0.advance(dt, reduced_motion)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn grid(layout: &Layout, len: usize) -> Grid {
        layout.grid(Rect::from_min_size(pos2(20.0, 30.0), layout.size), len)
    }

    #[test]
    fn gallery_fits_the_work_area_and_scales_the_artwork() {
        for (available, expected_columns) in [
            (vec2(1888.0, 1000.0), 3),
            (vec2(2528.0, 1000.0), 3),
            (vec2(1248.0, 648.0), 3),
            (vec2(992.0, 512.0), 2),
            (vec2(648.0, 400.0), 1),
        ] {
            let collapsed = vec2(1160.0, 500.0) * (available.x / 1160.0).min(1.0);
            for count in [0, 1, 2, 3, 6, 10, 30, 250] {
                let layout = Layout::new(available, collapsed, count);
                let grid = grid(&layout, count);
                let viewport = grid.viewport();
                assert!(layout.size.x <= available.x && layout.size.y <= available.y);
                assert_eq!(grid.columns, expected_columns.min(count.max(1)));
                assert!((grid.rect(0, 0.0).aspect_ratio() - 16.0 / 9.0).abs() < 1e-4);
                assert!(grid.visible(0.0).end <= count);
                if count > 0 {
                    let last = count - 1;
                    let rect = grid.rect(last, grid.reveal(last, 0.0));
                    assert!(viewport.expand(0.01).contains_rect(rect));
                }
            }
        }
    }

    #[test]
    fn desktop_window_fits_one_or_two_rows_and_stops_growing() {
        let plan = |count| Layout::new(vec2(1888.0, 1000.0), vec2(1160.0, 500.0), count);
        for count in 1..=3 {
            let layout = plan(count);
            assert_eq!(layout.size.y, 500.0);
            assert_eq!(grid(&layout, count).max_scroll(), 0.0);
        }
        let layout = plan(6);
        let six = grid(&layout, 6);
        assert!(six.tile_width > 480.0);
        assert!(six.max_scroll() < 0.01);
        assert!(layout.size.y > 600.0 && layout.size.y < 670.0);
        assert_eq!(plan(10).size, layout.size);
        assert_eq!(plan(250).size, layout.size);
    }

    #[test]
    fn navigation_stays_in_rows_and_handles_short_last_row() {
        let layout = Layout::new(vec2(1248.0, 648.0), vec2(1160.0, 500.0), 8);
        let grid = grid(&layout, 8);
        assert_eq!(grid.columns, 3);
        assert_eq!(grid.move_selection(1, Direction::Up), None);
        assert_eq!(grid.move_selection(2, Direction::Down), Some(5));
        assert_eq!(grid.move_selection(5, Direction::Down), Some(7));
        assert_eq!(grid.move_selection(7, Direction::Up), Some(4));
        assert_eq!(grid.move_selection(5, Direction::Right), Some(5));
        assert_eq!(grid.move_selection(3, Direction::Left), Some(3));
        assert_eq!(grid.move_selection(7, Direction::Down), Some(7));
    }

    #[test]
    fn reveals_selection_and_bounds_visible_work() {
        let layout = Layout::new(vec2(1888.0, 1000.0), vec2(1160.0, 500.0), 1000);
        let grid = grid(&layout, 1000);
        let viewport = grid.viewport();
        for index in [0, 25, 560, 999] {
            let scroll = grid.reveal(index, 0.0);
            assert!(grid.visible(scroll).contains(&index));
            let rect = grid.rect(index, scroll);
            assert!(rect.top() >= viewport.top() - 0.1);
            assert!(rect.bottom() <= viewport.bottom() + 0.1);
            assert!(grid.visible(scroll).len() <= 15);
        }
        assert_eq!(grid.reveal(0, grid.clamp_scroll(f32::MAX)), 0.0);
    }

    #[test]
    fn reversals_preserve_position_and_settle() {
        let mut transition = Transition::default();
        transition.target(true, false);
        assert!(transition.expanded() && transition.active());
        transition.advance(0.06, false);
        let before = transition.progress();
        assert!(before > 0.0 && before < 1.0);
        transition.target(false, false);
        assert_eq!(transition.progress(), before);
        assert!(!transition.advance(0.5, false));
        assert_eq!(transition.progress(), 0.0);
        transition.target(true, true);
        assert_eq!(transition.progress(), 1.0);
        assert!(!transition.active());
    }
}
