//! How a cost grows with its input, measured so that it says the same on any machine: a test
//! that hostile input costs no more than its bytes times its input at two sizes and bounds the
//! ratio, never the time. An absolute bound flakes on a slow or busy machine (the tests run
//! side by side on a debug build), and one loose enough not to can't tell linear from mildly
//! worse. A ratio can: a linear cost grows about [`FACTOR`] times from one size to the other, a
//! quadratic one its square.
//!
//! A ratio says nothing of a cost's constant: a pass that looks a fixed distance ahead at every
//! byte is linear, however far it looks.

use std::time::{Duration, Instant};

/// How many times larger the second input is than the first.
pub const FACTOR: usize = 4;

/// The most a cost may grow from one input to the other: [`FACTOR`] and room for noise, well
/// under the 16 times a quadratic cost grows.
pub const MOST_GROWTH: f64 = 6.0;

/// The most tries at each size.
const TRIES: usize = 5;

/// The least a time is taken over: a cost below it is run again until it is reached, so the
/// clock and the noise are small beside it.
const TIMED: Duration = Duration::from_millis(10);

/// What the whole measurement may take: a guard against a hang, never a bound on the cost.
pub const HANG: Duration = Duration::from_secs(60);

/**
 * Asserts that `run` costs about [`FACTOR`] times as much on `make(FACTOR * size)` as on
 * `make(size)`, and returns how much more it cost. Both inputs are made outside the timing.
 * Each size is timed up to [`TRIES`] times, the least time kept, stopping at the first ratio
 * under [`MOST_GROWTH`]: a test may be made to wait by the others running beside it. Pick
 * `size` so that both inputs are on the same side of every bound the code has on its input.
 */
pub fn assert_linear<T>(
    name: &str,
    size: usize,
    make: impl Fn(usize) -> T,
    mut run: impl FnMut(&T),
) -> f64 {
    let started = Instant::now();
    let (smaller, larger) = (make(size), make(size * FACTOR));
    let mut time = |input: &T, runs: u32| {
        let start = Instant::now();
        for _ in 0..runs {
            run(input);
        }
        start.elapsed() / runs
    };
    let once = time(&smaller, 1).as_nanos().max(1);
    let runs = u32::try_from(TIMED.as_nanos().div_ceil(once)).map_or(10_000, |it| it.min(10_000));
    let (mut small, mut large) = (Duration::MAX, Duration::MAX);
    let mut growth = f64::INFINITY;
    for _ in 0..TRIES {
        small = small.min(time(&smaller, runs));
        large = large.min(time(&larger, runs));
        growth = large.as_secs_f64() / small.as_secs_f64();
        if growth < MOST_GROWTH {
            break;
        }
    }
    eprintln!("{name:<44} {small:>10.1?} then {large:>10.1?}: {growth:.1}x");
    assert!(
        started.elapsed() < HANG,
        "{name}: measured in {:?}",
        started.elapsed()
    );
    assert!(
        growth < MOST_GROWTH,
        "{name}: {FACTOR} times the input took {growth:.1} times as long ({small:?}, then {large:?})"
    );
    growth
}
