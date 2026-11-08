//! A check that work grows in step with its input: twice the input taking
//! about twice as long, not four times, as work as its square would.

use std::time::Duration;

/// The fastest of each timing when the larger input took three times the
/// smaller and 50 ms more; none when it did not. Each size is timed again,
/// the two in turn, up to five times while the fastest of each says
/// otherwise, so a busy machine pausing the test once is not read as the
/// work growing. `timed(true)` times the larger input.
pub fn outgrows(mut timed: impl FnMut(bool) -> Duration) -> Option<(Duration, Duration)> {
    let (mut once, mut twice) = (Duration::MAX, Duration::MAX);
    for _ in 0..5 {
        once = once.min(timed(false));
        twice = twice.min(timed(true));
        if twice < once * 3 + Duration::from_millis(50) {
            return None;
        }
    }
    Some((once, twice))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn work_as_the_square_of_the_input_is_caught_however_often_it_is_timed() {
        let mut rounds = 0;
        let caught = outgrows(|larger| {
            rounds += usize::from(larger);
            Duration::from_millis(if larger { 320 } else { 80 })
        });
        let (once, twice) = (Duration::from_millis(80), Duration::from_millis(320));
        assert_eq!(caught, Some((once, twice)));
        assert_eq!(rounds, 5);
    }

    #[test]
    fn a_pause_while_timed_once_is_not_read_as_the_work_growing() {
        let mut timings = [300, 100, 60, 100].map(Duration::from_millis).into_iter();
        assert_eq!(outgrows(|_| timings.next().unwrap()), None);
    }
}
