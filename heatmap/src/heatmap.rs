// Copyright 2020 Twitter, Inc.
// Licensed under the Apache License, Version 2.0
// http://www.apache.org/licenses/LICENSE-2.0

use crate::window::OwnedWindow;
use crate::Error;
use crate::*;
use core::sync::atomic::*;

use histogram::{Bucket, Histogram};
use parking_lot::Mutex;

/// A `Heatmap` stores counts for timestamped values over a configured span of
/// time.
///
/// Internally, it is represented as a ring buffer of histograms with one
/// additional histogram to track all counts within the span of time. Old
/// histograms age-out as time moves forward and they are subtracted from the
/// summary histogram at that point.
///
/// This acts as a moving histogram, such that requesting a percentile returns
/// a percentile from across the configured span of time.
pub struct Heatmap {
    slices: Vec<OwnedWindow>,
    current: AtomicUsize,
    lock: Mutex<()>,
    start: AtomicInstant,
    stop: AtomicInstant,
    span: Duration,
    resolution: Duration,
    next_tick: AtomicInstant,
    summary: Histogram,
}

/// A `Builder` allows for constructing a `Heatmap` with the desired
/// configuration.
pub struct Builder {
    // minimum resolution parameter `M = 2^m`
    m: u32,
    // minimum resolution range parameter `R = 2^r - 1`
    r: u32,
    // maximum value parameter `N = 2^n - 1`
    n: u32,
    // span of time represented by the heatmap
    span: Duration,
    // the resolution in the time domain
    resolution: Duration,
}

impl Builder {
    /// Consume the `Builder` and return a `Heatmap`.
    pub fn build(self) -> Result<Heatmap, Error> {
        Heatmap::new(self.m, self.r, self.n, self.span, self.resolution)
    }

    /// Sets the width of the smallest bucket in the `Heatmap`.
    ///
    /// As the `Heatmap` uses base-2 internally, the resolution will be the
    /// largest power of two that is less than or equal to the provided value.
    /// For example, if the minimum resolution is set to 10, the width of the
    /// smallest bucket will be 8.
    pub fn min_resolution(mut self, width: u64) -> Self {
        self.m = 64 - width.leading_zeros();
        self
    }

    /// Sets the maximum value that the minimum resolution extends to.
    ///
    /// This value should be greater than the minimum resolution. If the value
    /// provided is not a power of two, the smallest power of two that is larger
    /// than the provided value will be used.
    pub fn min_resolution_range(mut self, value: u64) -> Self {
        self.r = 64 - value.next_power_of_two().leading_zeros();
        self
    }

    /// Sets the maximum value that can be recorded into the `Heatmap`.
    ///
    /// If the value provided is not a power of two, the smallest power of two
    /// that is larger than the provided value will be used.
    pub fn maximum_value(mut self, value: u64) -> Self {
        self.n = 64 - value.next_power_of_two().leading_zeros();
        self
    }

    /// Sets the duration that is covered by the `Heatmap`.
    ///
    /// Values that are older than the duration will be dropped as they age-out.
    pub fn span(mut self, duration: Duration) -> Self {
        self.span = duration;
        self
    }

    /// Sets the resolution in the time domain.
    ///
    /// Increments with similar timestamps will be grouped together and age-out
    /// together.
    pub fn resolution(mut self, duration: Duration) -> Self {
        self.resolution = duration;
        self
    }
}

impl Heatmap {
    /// Create a new `Heatmap` which stores counts for timestamped values over
    /// a configured span of time.
    ///
    /// - `m` - sets the minimum resolution `M = 2^m`. This is the smallest unit
    /// of quantification, which is also the smallest bucket width. If the input
    /// values are always integers, choosing `m=0` would ensure precise
    /// recording for the smallest values.
    ///
    /// - `r` - sets the minimum resolution range `R = 2^r - 1`. The selected
    /// value must be greater than the minimum resolution `m`. This sets the
    /// maximum value that the minimum resolution should extend to.
    ///
    /// - `n` - sets the maximum value `N = 2^n - 1`. The selected value must be
    /// greater than or equal to the minimum resolution range `r`.
    ///
    /// - `span` - sets the total duration that the heatmap covers
    ///
    /// - `resolution` - sets the resolution in the time domain. Counts from
    /// similar instants in time will be grouped together.
    pub fn new(
        m: u32,
        r: u32,
        n: u32,
        span: Duration,
        resolution: Duration,
    ) -> Result<Self, Error> {
        let now = Instant::now();

        let mut slices = Vec::new();
        let mut true_span = Duration::from_nanos(0);
        while true_span < span {
            slices.push(OwnedWindow {
                start: AtomicInstant::new(now + true_span),
                stop: AtomicInstant::new(now + true_span + resolution),
                histogram: Histogram::new(m, r, n)?,
            });
            true_span += resolution;
        }
        // allocate one extra histogram so we always have a cleared
        // one in the ring
        slices.push(OwnedWindow {
            start: AtomicInstant::new(now + true_span),
            stop: AtomicInstant::new(now + true_span + resolution),
            histogram: Histogram::new(m, r, n)?,
        });
        slices.shrink_to_fit();

        let start = AtomicInstant::new(now);
        let stop = AtomicInstant::new(now + true_span);
        let next_tick = AtomicInstant::new(now + resolution);

        Ok(Self {
            slices,
            current: AtomicUsize::new(0),
            lock: Mutex::new(()),
            start,
            stop,
            span: true_span,
            resolution,
            next_tick,
            summary: Histogram::new(m, r, n)?,
        })
    }

    /// Creates a `Builder` with the default values `m = 0`, `r = 10`, `n = 30`,
    /// `span = 60s`, `resolution = 1s`.
    ///
    /// This would create a `Heatmap` with 61 total `Histogram`s, each with
    /// 11264 buckets which can store values from 1 to 1_073_741_823 with
    /// values 1 to 1023 being stored in buckets with a width of 1. Such a
    /// `Heatmap` would be appropriate for latencies measured in nanoseconds
    /// where the max expected latency is one second and reporting covers the
    /// past minute.
    pub fn builder() -> Builder {
        Builder {
            m: 0,
            r: 10,
            n: 30,
            span: Duration::from_secs(60),
            resolution: Duration::from_secs(1),
        }
    }

    /// Returns the number of windows stored in the `Heatmap`
    pub fn windows(&self) -> usize {
        self.slices.len()
    }

    /// Returns the number of buckets stored within each `Histogram` in the
    /// `Heatmap`
    pub fn buckets(&self) -> usize {
        self.summary.buckets()
    }

    /// Increment a time-value pair by a specified count
    pub fn increment(&self, time: Instant, value: u64, count: u32) {
        self.tick(time);

        let current = self.current.load(Ordering::Relaxed);

        // fast path when the time falls into the current time slice
        if time >= self.slices[current].start.load(Ordering::Relaxed) {
            let _ = self.summary.increment(value, count);
            let _ = self.slices[current].histogram.increment(value, count);
        }

        // since it's before the current time slice, we need to record it into
        // a past slice

        let start = self.start.load(Ordering::Relaxed);
        let stop = self.stop.load(Ordering::Relaxed);

        // if the sample is before the heatmap start, we drop it
        if time < start {
            return;
        }

        // it's easier to work backwards from the current slice
        let offset = ((stop - time).as_nanos() / self.resolution.as_nanos()) as usize;

        let index = if offset > current {
            current + self.slices.len() - offset
        } else {
            current - offset
        };

        let _ = self.summary.increment(value, count);
        let _ = self.slices[index].histogram.increment(value, count);
    }

    /// Return the nearest value for the requested percentile (0.0 - 100.0)
    /// across the total range of samples retained in the `Heatmap`.
    ///
    /// Note: since the heatmap stores a distribution across a configured time
    /// span, sequential calls to fetch the percentile might result in different
    /// results even without concurrent writers. For instance, you may see a
    /// 90th percentile that is higher than the 100th percentile depending on
    /// the timing of calls to this function and the distribution of your data.
    ///
    /// Note: concurrent writes may also effect the value returned by this
    /// function. Users needing better consistency should ensure that other
    /// threads are not writing into the heatmap while this function is
    /// in-progress.
    pub fn percentile(&self, percentile: f64) -> Result<Bucket, Error> {
        self.tick(Instant::now());
        self.summary.percentile(percentile).map_err(Error::from)
    }

    /// Creates an iterator to iterate over the component histograms of this
    /// heatmap.
    pub fn iter(&self) -> Iter {
        self.into_iter()
    }

    /// Access the summary histogram of this heatmap.
    ///
    /// Note that concurrent modifications to the heatmap will continue to show
    /// up in the summary histogram while it is being read so sequential
    /// queries may not return consistent results.
    pub fn summary(&self) -> &Histogram {
        &self.summary
    }

    // Internal function which handles reuse of older windows to store newer
    /// values.
    fn tick(&self, time: Instant) {
        loop {
            // quick check to see if this data point requires ticking forward
            let next_tick = self.next_tick.load(Ordering::Relaxed);
            if time < next_tick {
                return;
            } else {
                // some expiration needs to happen, let's try to acquire the lock
                //
                // note: we use parking_lot mutex as it will not be poisoned by
                // a thread panic while locked.
                if let Some(_lock) = self.lock.try_lock() {
                    // now that we have the lock, check that we still need to
                    // tick forward
                    if time < self.next_tick.load(Ordering::Relaxed) {
                        return;
                    }

                    // calculate the index of the next current slice
                    let current = self.current.load(Ordering::Relaxed);
                    let mut next = current + 1;
                    if next >= self.slices.len() {
                        next -= self.slices.len();
                    }

                    // move current and next_tick forward
                    self.current.store(next, Ordering::Relaxed);
                    self.next_tick.fetch_add(self.resolution, Ordering::Relaxed);

                    // now we have a slice to subtract and clear from the summary
                    // this is the histogram that is one ahead of our new current
                    // position
                    let mut to_clear = next + 1;

                    // check if we need to wrap around to the start
                    if to_clear >= self.slices.len() {
                        to_clear -= self.slices.len();
                    }

                    self.start.fetch_add(self.resolution, Ordering::Relaxed);
                    self.stop.fetch_add(self.resolution, Ordering::Relaxed);

                    // check to make sure it's actually an old slice. A newly
                    // created heatmap will have all slices in the future.
                    if self.slices[to_clear].start.load(Ordering::Relaxed)
                        < self.start.load(Ordering::Relaxed)
                    {
                        // subtract and clear
                        let _ = self
                            .summary
                            .subtract_and_clear(&self.slices[to_clear].histogram);

                        self.slices[to_clear]
                            .start
                            .fetch_add(self.span, Ordering::Relaxed);
                        self.slices[to_clear]
                            .stop
                            .fetch_add(self.span, Ordering::Relaxed);
                    }
                }
                // if we failed to acquire the lock, just loop. this does mean
                // we busy wait if the heatmap has fallen behind by multiple
                // ticks. we expect the typical case to be that we need to tick
                // forward by just a single slice. in that case, if we fail to
                // acquire the lock, we expect that the loop will terminate when
                // we check `next_tick` at the start of the next iteration.
            }
        }
    }

    /// Internal function to return a `Window` from the `Heatmap`.
    fn get_slice(&self, index: usize) -> Option<Window> {
        if let Some(histogram) = self.slices.get(index) {
            let shift = if index > self.current.load(Ordering::Relaxed) {
                self.resolution.mul_f64(
                    (self.slices.len() + self.current.load(Ordering::Relaxed) - index) as f64,
                )
            } else {
                self.resolution
                    .mul_f64((self.current.load(Ordering::Relaxed) - index) as f64)
            };
            Some(Window {
                start: self.next_tick.load(Ordering::Relaxed) - shift - self.resolution,
                stop: self.next_tick.load(Ordering::Relaxed) - shift,
                histogram: &histogram.histogram,
            })
        } else {
            None
        }
    }
}

impl Clone for Heatmap {
    fn clone(&self) -> Self {
        let slices = self.slices.clone();
        let summary = self.summary.clone();
        let start = AtomicInstant::new(self.start.load(Ordering::Relaxed));
        let stop = AtomicInstant::new(self.stop.load(Ordering::Relaxed));
        let span = self.span;
        let resolution = self.resolution;
        let current = AtomicUsize::new(self.current.load(Ordering::Relaxed));
        let next_tick = AtomicInstant::new(self.next_tick.load(Ordering::Relaxed));

        Heatmap {
            slices,
            current,
            lock: Mutex::new(()),
            start,
            stop,
            span,
            resolution,
            next_tick,
            summary,
        }
    }
}

pub struct Iter<'a> {
    inner: &'a Heatmap,
    index: usize,
    visited: usize,
}

impl<'a> Iter<'a> {
    fn new(inner: &'a Heatmap) -> Iter<'a> {
        let index = if inner.current.load(Ordering::Relaxed) < (inner.slices.len() - 1) {
            inner.current.load(Ordering::Relaxed) + 1
        } else {
            0
        };
        Iter {
            inner,
            index,
            visited: 0,
        }
    }
}

impl<'a> Iterator for Iter<'a> {
    type Item = Window<'a>;

    fn next(&mut self) -> Option<Window<'a>> {
        if self.visited >= self.inner.slices.len() {
            None
        } else {
            let bucket = self.inner.get_slice(self.index);
            self.index += 1;
            if self.index >= self.inner.slices.len() {
                self.index = 0;
            }
            self.visited += 1;
            bucket
        }
    }
}

impl<'a> IntoIterator for &'a Heatmap {
    type Item = Window<'a>;
    type IntoIter = Iter<'a>;

    fn into_iter(self) -> Self::IntoIter {
        Iter::new(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn age_out() {
        let heatmap =
            Heatmap::new(0, 4, 20, Duration::from_secs(1), Duration::from_millis(1)).unwrap();
        assert_eq!(heatmap.percentile(0.0).map(|v| v.high()), Err(Error::Empty));
        heatmap.increment(Instant::now(), 1, 1);
        assert_eq!(heatmap.percentile(0.0).map(|v| v.high()), Ok(1));
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(heatmap.percentile(0.0).map(|v| v.high()), Ok(1));
        std::thread::sleep(std::time::Duration::from_millis(2000));
        assert_eq!(heatmap.percentile(0.0).map(|v| v.high()), Err(Error::Empty));
    }
}
