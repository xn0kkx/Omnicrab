//! The crab mascot shown in the chat. Pure functions of the tick counter.

pub const WIDTH: usize = 13;
pub const HEIGHT: usize = 4;

type Frame = [&'static str; HEIGHT];

const CLAWS_UP: Frame = [
    r"(\/)_____(\/)",
    r"  \ (o o) /  ",
    r"   ( ___ )   ",
    r" _/|     |\_ ",
];
const CLAWS_DOWN: Frame = [
    r"(/\)_____(/\)",
    r"  / (o o) \  ",
    r"   ( ___ )   ",
    r" \_|     |_/ ",
];
const BLINK: Frame = [
    r"(\/)_____(\/)",
    r"  \ (- -) /  ",
    r"   ( ___ )   ",
    r" _/|     |\_ ",
];

/// The crab's pose and its column offset inside a strip `width` columns wide.
/// Idle: sits in the middle and blinks now and then. Running: scuttles from
/// side to side, snapping its claws.
pub fn frame(tick: u64, running: bool, width: usize) -> (&'static Frame, usize) {
    let span = width.saturating_sub(WIDTH);
    if !running {
        let pose = if tick % 40 < 2 { &BLINK } else { &CLAWS_UP };
        return (pose, span / 2);
    }
    // Ping-pong: 0..=span..0, one column per tick.
    let period = (span * 2).max(1) as u64;
    let p = (tick % period) as usize;
    let offset = if p <= span { p } else { span * 2 - p };
    let pose = if (tick / 2).is_multiple_of(2) {
        &CLAWS_UP
    } else {
        &CLAWS_DOWN
    };
    (pose, offset)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_have_equal_width() {
        for f in [&CLAWS_UP, &CLAWS_DOWN, &BLINK] {
            for l in f.iter() {
                assert_eq!(l.chars().count(), WIDTH, "{l:?}");
            }
        }
    }

    #[test]
    fn idle_is_still_and_running_moves_within_bounds() {
        let w = 60;
        let idle: Vec<_> = (0..10).map(|t| frame(t, false, w).1).collect();
        assert!(idle.iter().all(|o| *o == idle[0]));
        let mut seen = std::collections::HashSet::new();
        for t in 0..200 {
            let (_, o) = frame(t, true, w);
            assert!(o + WIDTH <= w);
            seen.insert(o);
        }
        assert!(seen.len() > 10);
        assert_ne!(frame(0, true, w).0, frame(2, true, w).0);
    }

    #[test]
    fn narrow_strip_does_not_panic() {
        assert_eq!(frame(5, true, 3).1, 0);
        assert_eq!(frame(5, false, 0).1, 0);
    }
}
