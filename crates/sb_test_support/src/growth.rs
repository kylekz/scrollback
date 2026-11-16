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
pub const FACTOR: usize = 8;

/// The most a cost may grow from one input to the other: twice [`FACTOR`], for noise, and a
/// quarter of the 64 times a quadratic cost grows.
pub const MOST_GROWTH: f64 = 16.0;

/// The most tries, each timing both inputs; the ratio kept is their median.
const TRIES: usize = 5;

/// The least a sample is timed over: a run shorter than it is repeated until it is reached, so
/// that the clock, and a moment's wait, are small beside it.
const SAMPLE: Duration = Duration::from_millis(5);

/// What the whole measurement may take: a guard against a hang, never a bound on the cost.
pub const HANG: Duration = Duration::from_secs(60);

/**
 * Asserts that `run` costs about [`FACTOR`] times as much on `make(FACTOR * size)` as on
 * `make(size)`, and returns how much more it cost. Both inputs are made outside the timing.
 *
 * Each try times the smaller input and then the larger, each run as many times as it takes to
 * fill [`SAMPLE`], and the ratio of the two is the try's. The median of up to [`TRIES`] tries
 * is bounded, and the tries stop once most of them agree: a test may be made to wait by the
 * others beside it, now and then, but not for most of its tries. Pick `size` so that both
 * inputs are on the same side of every bound the code has on its input.
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
    /* Each run once, which also warms what the runs share; then as many runs as fill a sample. */
    let runs = |once: Duration| {
        u32::try_from(SAMPLE.as_nanos().div_ceil(once.as_nanos().max(1))).unwrap_or(u32::MAX)
    };
    let (small_runs, large_runs) = (runs(time(&smaller, 1)), runs(time(&larger, 1)));
    let mut tries: Vec<(f64, Duration, Duration)> = Vec::with_capacity(TRIES);
    let most = TRIES / 2 + 1;
    while tries.len() < TRIES {
        let small = time(&smaller, small_runs);
        let large = time(&larger, large_runs);
        tries.push((large.as_secs_f64() / small.as_secs_f64(), small, large));
        let under = tries.iter().filter(|it| it.0 < MOST_GROWTH).count();
        if under >= most || tries.len() - under >= most {
            break;
        }
    }
    tries.sort_by(|a, b| a.0.total_cmp(&b.0));
    /* The median of the tries made, which stopped once most were on one side of the bound. */
    let (growth, small, large) = tries[tries.len() / 2];
    eprintln!(
        "{name:<44} {small:>10.1?} then {large:>10.1?}: {growth:.1}x (of {} tries)",
        tries.len()
    );
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

#[cfg(test)]
mod tests {
    use std::hint::black_box;

    use super::*;

    /// A pass over the bytes, and one over each pair of them.
    fn linear(bytes: &[u8]) -> u64 {
        bytes.iter().map(|it| u64::from(*it)).sum()
    }

    fn quadratic(bytes: &[u8]) -> u64 {
        let mut pairs = 0;
        for (at, a) in bytes.iter().enumerate() {
            for b in &bytes[at..] {
                pairs += u64::from(a ^ b);
            }
        }
        pairs
    }

    #[test]
    fn a_linear_cost_passes() {
        assert_linear(
            "linear",
            64 * 1024,
            |size| vec![7u8; size],
            |bytes| {
                black_box(linear(black_box(bytes)));
            },
        );
    }

    #[test]
    #[should_panic(expected = "times the input took")]
    fn a_quadratic_cost_fails() {
        assert_linear(
            "quadratic",
            1_000,
            |size| vec![7u8; size],
            |bytes| {
                black_box(quadratic(black_box(bytes)));
            },
        );
    }
}
