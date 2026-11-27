//! Where the progress bar stands.
//!
//! Nothing reports how far a download or an installation is, so the bar
//! cannot be exact. It must still move, or the user takes a still bar for
//! a hung update. Each step owns a stretch of the bar, and within it the bar
//! creeps toward the stretch's end, quickly at first and then ever slower,
//! so it never reaches the end of a step that has not ended.

use std::time::Duration;

use crate::watch::Step;

/// The bar's range: per mille, so that the creep is smooth.
pub const BAR_END: u32 = 1000;

/// The stretch of the bar a step owns, and how fast it creeps there: in
/// `pace`, it covers about two thirds of its stretch.
struct Stretch {
    start: u32,
    end: u32,
    pace: Duration,
}

fn stretch(step: Step) -> Stretch {
    // Paces from the reference machine: a download of about 30 MB and a
    // Windows Installer run of half a minute.
    match step {
        Step::Preparing => Stretch { start: 0, end: 400, pace: Duration::from_secs(15) },
        Step::Installing => Stretch { start: 400, end: 850, pace: Duration::from_secs(25) },
        Step::Restarting => Stretch { start: 850, end: 980, pace: Duration::from_secs(6) },
    }
}

/// The bar's position, out of [`BAR_END`], for `step` reached `in_step` ago.
pub fn position(step: Step, in_step: Duration) -> u32 {
    let Stretch { start, end, pace } = stretch(step);
    let covered = 1.0 - (-in_step.as_secs_f64() / pace.as_secs_f64()).exp();
    // `covered` tends to 1 but rounds to it after a long wait; the end of a
    // step's stretch is reached only when the step is over.
    (start + (f64::from(end - start) * covered) as u32).min(end - 1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_step_starts_where_the_one_before_ends() {
        let mut previous_end = 0;
        for step in Step::ALL {
            let Stretch { start, end, .. } = stretch(step);
            assert_eq!(start, previous_end, "(stretch of {step:?}): expected no gap or overlap");
            assert!(start < end && end < BAR_END, "(stretch of {step:?}): {start}..{end}");
            previous_end = end;
        }
    }

    #[test]
    fn the_bar_moves_forward_but_never_past_its_step() {
        for step in Step::ALL {
            let Stretch { start, end, .. } = stretch(step);
            assert_eq!(position(step, Duration::ZERO), start, "{step:?}");
            let mut last = start;
            for second in [1, 2, 5, 10, 30, 60, 600, 3600] {
                let now = position(step, Duration::from_secs(second));
                assert!(
                    now >= last && now < end,
                    "(bar in {step:?} after {second} s): expected {last}..{end} | received: {now}"
                );
                last = now;
            }
        }
    }

    #[test]
    fn the_bar_visibly_moves_within_a_few_seconds() {
        for step in Step::ALL {
            let moved = position(step, Duration::from_secs(2)) - position(step, Duration::ZERO);
            // A pixel of a 400-pixel bar is 2.5 per mille.
            assert!(moved >= 10, "(bar in {step:?} after 2 s): moved {moved} per mille");
        }
    }
}
