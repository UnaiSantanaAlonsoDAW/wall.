//! Frame-rate-independent motion and discrete input for the wallpaper strip.
//! Rendering derives every image transform from `position`; `target` is the
//! selected image and is the only index that Enter should apply.

const FREQUENCY: f64 = 30.0;
const POSITION_EPSILON: f64 = 0.0005;
const VELOCITY_EPSILON: f64 = 0.015;
const POINTS_PER_ITEM: f64 = 72.0;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WheelUnit {
    /// One logical wheel line advances one image; fractional lines accumulate.
    Lines,
    /// Logical pixels from a precision trackpad, before egui scroll smoothing.
    Points,
}

#[derive(Debug)]
pub struct Carousel {
    pub target: usize,
    pub position: f64,
    velocity: f64,
    len: usize,
    wheel_remainder: f64,
}

impl Carousel {
    pub fn new(index: usize) -> Self {
        Self {
            target: index,
            position: index as f64,
            velocity: 0.0,
            len: index.saturating_add(1),
            wheel_remainder: 0.0,
        }
    }

    /// Select without restarting the spring or discarding its current velocity.
    /// Returns whether the selected index changed. Collections do not wrap.
    pub fn select(&mut self, index: usize, len: usize) -> bool {
        self.wheel_remainder = 0.0;
        self.set_target(index, len)
    }

    pub fn step(&mut self, delta: isize, len: usize) -> bool {
        self.select(self.target.saturating_add_signed(delta), len)
    }

    fn set_target(&mut self, index: usize, len: usize) -> bool {
        let previous = self.target;
        self.len = len;
        self.target = index.min(len.saturating_sub(1));
        if len <= 1 {
            self.position = 0.0;
            self.velocity = 0.0;
        }
        previous != self.target
    }

    /// Positive delta means next image; the UI normalizes raw event direction.
    /// Feed raw egui wheel events once, not smooth_scroll_delta every frame.
    /// A direction reversal discards the old partial gesture so the new gesture
    /// is not fighting accumulated input in the opposite direction.
    pub fn wheel(&mut self, delta: f32, unit: WheelUnit, len: usize) -> bool {
        if !delta.is_finite() || delta == 0.0 {
            return false;
        }
        let delta = match unit {
            WheelUnit::Lines => f64::from(delta),
            WheelUnit::Points => f64::from(delta) / POINTS_PER_ITEM,
        };
        if delta.signum() != self.wheel_remainder.signum() {
            self.wheel_remainder = 0.0;
        }
        self.wheel_remainder += delta;
        // Epsilon avoids losing an integer notch split across fractional events.
        let steps = (self.wheel_remainder.abs() + 1e-9).floor() * self.wheel_remainder.signum();
        if steps == 0.0 {
            return false;
        }
        self.wheel_remainder -= steps;
        let changed = self.set_target(self.target.saturating_add_signed(steps as isize), len);
        if (self.target == 0 && self.wheel_remainder < 0.0)
            || (self.target == len.saturating_sub(1) && self.wheel_remainder > 0.0)
        {
            self.wheel_remainder = 0.0;
        }
        changed
    }

    /// Exact solution of x'' + 2ωx' + ω²(x-target) = 0 for this elapsed time.
    /// Unlike frame-wise lerp or Euler integration, a slow frame cannot make
    /// this spring unstable. A normal step is 98% complete at about 200 ms.
    /// Returns whether another animation frame is needed after this step.
    pub fn advance(&mut self, dt: f64, reduced_motion: bool) -> bool {
        if reduced_motion {
            // Reduced motion affects visual interpolation, not the user's
            // in-progress precision-scroll gesture. Explicit snap() is reserved
            // for lifecycle/selection resets and deliberately clears gestures.
            self.position = self.target as f64;
            self.velocity = 0.0;
            return false;
        }
        if self.is_settled() {
            self.position = self.target as f64;
            self.velocity = 0.0;
            return false;
        }
        if !dt.is_finite() || dt <= 0.0 {
            return true;
        }
        let target = self.target as f64;
        let displacement = self.position - target;
        let coefficient = self.velocity + FREQUENCY * displacement;
        let decay = (-FREQUENCY * dt).exp();
        let next = target + (displacement + coefficient * dt) * decay;
        let velocity = (self.velocity - FREQUENCY * coefficient * dt) * decay;

        // Retargeting preserves momentum. If that momentum would carry a card
        // through its newly selected center, stop exactly at the center instead
        // of showing a small overshoot and returning. Reversals still carry the
        // existing velocity until the spring turns them around naturally.
        let crossed_target = displacement == 0.0 || (next - target) * displacement <= 0.0;
        let bounded = next.clamp(0.0, self.len.saturating_sub(1) as f64);
        if crossed_target || bounded != next || !next.is_finite() || !velocity.is_finite() {
            self.position = if crossed_target || !next.is_finite() {
                target
            } else {
                bounded
            };
            self.velocity = 0.0;
        } else {
            self.position = next;
            self.velocity = velocity;
        }
        if self.is_settled() {
            self.position = target;
            self.velocity = 0.0;
            false
        } else {
            true
        }
    }

    pub fn snap(&mut self, index: usize) {
        self.target = index;
        self.position = index as f64;
        self.velocity = 0.0;
        self.len = self.len.max(index.saturating_add(1));
        self.wheel_remainder = 0.0;
    }

    pub fn is_settled(&self) -> bool {
        (self.position - self.target as f64).abs() <= POSITION_EPSILON
            && self.velocity.abs() <= VELOCITY_EPSILON
    }

    /// Keep the selected file's spatial displacement when an insertion/removal
    /// changes its index. The caller resolves identity using the file path.
    /// Retargeting or restarting the animation here would create an import jump.
    pub fn rebase_after_collection_change(
        &mut self,
        old_selected_index: usize,
        new_selected_index: usize,
        len: usize,
    ) {
        let displacement = self.position - old_selected_index as f64;
        self.len = len;
        self.target = new_selected_index.min(len.saturating_sub(1));
        if len <= 1 {
            self.snap(0);
            self.len = len;
            return;
        }
        let translated = self.target as f64 + displacement;
        self.position = translated.clamp(0.0, len.saturating_sub(1) as f64);
        if self.position != translated {
            self.velocity = 0.0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spring_position_and_velocity_are_frame_rate_independent() {
        let mut sixty = Carousel::new(0);
        let mut one_twenty = Carousel::new(0);
        sixty.select(5, 10);
        one_twenty.select(5, 10);
        for _ in 0..12 {
            sixty.advance(1.0 / 60.0, false);
        }
        for _ in 0..24 {
            one_twenty.advance(1.0 / 120.0, false);
        }
        assert!((sixty.position - one_twenty.position).abs() < 1e-12);
        assert!((sixty.velocity - one_twenty.velocity).abs() < 1e-11);
        assert!(sixty.position > 4.9 && sixty.position < 5.0);
    }

    #[test]
    fn retargeting_and_reversal_carry_position_and_velocity() {
        let mut carousel = Carousel::new(0);
        carousel.select(3, 8);
        carousel.advance(0.04, false);
        let position = carousel.position;
        let velocity = carousel.velocity;
        carousel.select(5, 8);
        assert_eq!(carousel.position, position);
        assert_eq!(carousel.velocity, velocity);
        carousel.advance(0.03, false);
        let position = carousel.position;
        let velocity = carousel.velocity;
        carousel.select(0, 8);
        assert_eq!(carousel.position, position);
        assert_eq!(carousel.velocity, velocity);
        carousel.advance(0.001, false);
        assert!(
            carousel.position > position,
            "Momentum is carried through a reversal"
        );
        for _ in 0..120 {
            carousel.advance(1.0 / 120.0, false);
        }
        assert!(carousel.is_settled());
        assert_eq!(carousel.position, 0.0);
    }

    #[test]
    fn empty_single_and_two_image_collections_stop_at_boundaries() {
        let mut carousel = Carousel::new(0);
        assert!(!carousel.step(1, 0));
        assert!(!carousel.advance(0.1, false));
        assert!(!carousel.step(-1, 1));
        assert!(!carousel.step(1, 1));
        assert!(carousel.step(1, 2));
        assert!(!carousel.step(1, 2));
        carousel.advance(1.0, false);
        assert_eq!(carousel.position, 1.0);
        assert!(carousel.step(isize::MIN, 2));
        assert_eq!(carousel.target, 0);
        assert!(!carousel.step(-1, 2));
    }

    #[test]
    fn reduced_motion_snaps_exactly_without_active_frames() {
        let mut carousel = Carousel::new(0);
        carousel.select(4, 5);
        carousel.advance(0.02, false);
        assert!(!carousel.advance(0.0, true));
        assert_eq!(carousel.position, 4.0);
        assert_eq!(carousel.velocity, 0.0);
        assert!(carousel.is_settled());
    }

    #[test]
    fn reduced_motion_preserves_precision_wheel_accumulation_between_frames() {
        let mut carousel = Carousel::new(0);
        for frame in 0..4 {
            assert_eq!(carousel.wheel(18.0, WheelUnit::Points, 5), frame == 3);
            assert!(!carousel.advance(1.0 / 60.0, true));
        }
        assert_eq!(carousel.target, 1);
        assert_eq!(carousel.position, 1.0);
        assert!(carousel.is_settled());
    }

    #[test]
    fn rebasing_preserves_animation_offset_when_selection_moves_in_collection() {
        let mut carousel = Carousel::new(3);
        carousel.select(5, 9);
        carousel.advance(0.04, false);
        let offset = carousel.position - 5.0;
        let velocity = carousel.velocity;
        carousel.rebase_after_collection_change(5, 4, 8);
        assert_eq!(carousel.target, 4);
        assert!((carousel.position - 4.0 - offset).abs() < 1e-12);
        assert_eq!(carousel.velocity, velocity);
        carousel.rebase_after_collection_change(4, 0, 0);
        assert_eq!(carousel.position, 0.0);
        assert!(carousel.is_settled());
    }

    #[test]
    fn wheel_notches_and_trackpad_points_accumulate_and_reverse_cleanly() {
        let mut carousel = Carousel::new(0);
        assert!(carousel.wheel(1.0, WheelUnit::Lines, 10));
        assert_eq!(carousel.target, 1);
        for _ in 0..3 {
            assert!(!carousel.wheel(18.0, WheelUnit::Points, 10));
        }
        assert!(carousel.wheel(18.0, WheelUnit::Points, 10));
        assert_eq!(carousel.target, 2);
        assert!(!carousel.wheel(35.0, WheelUnit::Points, 10));
        assert!(carousel.wheel(-72.0, WheelUnit::Points, 10));
        assert_eq!(carousel.target, 1);
        assert!(!carousel.wheel(f32::NAN, WheelUnit::Lines, 10));
        assert!(carousel.wheel(-100.0, WheelUnit::Lines, 10));
        assert_eq!(carousel.target, 0);
        assert!(carousel.wheel(1.0, WheelUnit::Lines, 10));
        assert_eq!(carousel.target, 1);
    }

    #[test]
    fn slow_frames_and_rapid_retargets_do_not_overshoot_or_leave_nonfinite_state() {
        let mut carousel = Carousel::new(0);
        carousel.select(6, 8);
        carousel.advance(0.05, false);
        let near = carousel.position.ceil() as usize;
        carousel.select(near, 8);
        for _ in 0..30 {
            carousel.advance(1.0 / 60.0, false);
            assert!(carousel.position <= near as f64);
        }
        carousel.select(7, 8);
        assert!(!carousel.advance(1000.0, false));
        assert_eq!(carousel.position, 7.0);
        carousel.select(0, 8);
        assert!(carousel.advance(f64::NAN, false));
        assert!(carousel.position.is_finite() && carousel.velocity.is_finite());
    }
}
