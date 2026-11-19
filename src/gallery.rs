//! Logical grid coordinates independent of HWND size and scroll events.

use crate::carousel::Carousel;
use eframe::egui::{Rect, pos2, vec2};
use std::ops::Range;

const GAP: f32 = 14.0;
const MIN_TILE_WIDTH: f32 = 216.0;
const MAX_TILE_WIDTH: f32 = 282.0;
const MAX_COLUMNS: usize = 5;
const OVERSCAN_ROWS: usize = 1;

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
    left: f32,
    top: f32,
}

impl Grid {
    pub fn new(bounds: Rect, len: usize) -> Self {
        let width = bounds.width().max(0.0);
        let columns = (((width + GAP) / (MIN_TILE_WIDTH + GAP)) as usize)
            .clamp(1, MAX_COLUMNS)
            .min(len.max(1));
        let gaps = (columns - 1) as f32 * GAP;
        let tile_width = ((width - gaps) / columns as f32).min(MAX_TILE_WIDTH);
        let total_width = columns as f32 * tile_width + gaps;
        let rows = len.div_ceil(columns);
        let tile_height = tile_width * 9.0 / 16.0;
        let content_height = rows as f32 * (tile_height + GAP) - GAP;
        Self {
            columns,
            bounds,
            len,
            rows,
            tile_width,
            tile_height,
            left: bounds.center().x - total_width * 0.5,
            top: bounds.top() + (bounds.height() - content_height).max(0.0) * 0.5,
        }
    }

    pub fn rect(&self, index: usize, scroll: f32) -> Rect {
        let x = self.left + (index % self.columns) as f32 * (self.tile_width + GAP);
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
        let content_height = self.rows as f32 * self.row_pitch() - GAP;
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
        self.tile_height + GAP
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

    fn bounds(width: f32, height: f32) -> Rect {
        Rect::from_min_size(pos2(20.0, 30.0), vec2(width, height))
    }

    #[test]
    fn grid_is_responsive_and_landscape() {
        assert!(
            Grid::new(bounds(600.0, 460.0), 20).columns
                < Grid::new(bounds(1420.0, 640.0), 20).columns
        );
        for count in [0, 1, 2, 10, 50, 250] {
            let viewport = bounds(1420.0, 640.0);
            let grid = Grid::new(viewport, count);
            let rect = grid.rect(0, 0.0);
            assert!((rect.aspect_ratio() - 16.0 / 9.0).abs() < 1e-4);
            let visible = grid.visible(0.0);
            assert_eq!(visible.start, 0);
            assert!(visible.end <= count);
            if count > 0 {
                assert!(viewport.contains_rect(grid.rect(grid.columns - 1, 0.0)));
            }
        }
    }

    #[test]
    fn navigation_stays_in_rows_and_handles_short_last_row() {
        let grid = Grid::new(bounds(750.0, 320.0), 8);
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
        let viewport = bounds(1420.0, 660.0);
        let grid = Grid::new(viewport, 1000);
        for index in [0, 25, 560, 999] {
            let scroll = grid.reveal(index, 0.0);
            assert!(grid.visible(scroll).contains(&index));
            let rect = grid.rect(index, scroll);
            assert!(rect.top() >= viewport.top() - 0.1);
            assert!(rect.bottom() <= viewport.bottom() + 0.1);
            assert!(grid.visible(scroll).len() < 60);
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
