//! Decides which render workers get which frame, and when a frame is whole.
//!
//! Deliberately platform-free: no `Worker`, no `postMessage`, no canvas. The
//! DOM-facing pool in `drivers::render_worker_pool` owns those and asks this
//! type what to do, the same split `GenerationWorkerRequests` uses for the
//! chunk workers. It is the only part of the threading design that can be
//! tested without a browser, so it is where the tricky rules live.
//!
//! Two rules shape everything here:
//!
//! * **Present whole frames only.** Bands finish at different times, so
//!   drawing each one as it lands would show several different moments of
//!   the world stacked on top of each other — visible as horizontal tearing
//!   between bands whenever the camera moves. Instead bands are buffered and
//!   composited when every band has the same frame, and until then the last
//!   complete frame stays on screen.
//! * **One frame at a time across the whole pool.** A new frame starts only
//!   when every healthy band is idle.
//!
//! The second rule exists because of the first, and the pair is easy to get
//! wrong: handing a new frame to whichever band happens to be free means the
//! bands end up on *different* frame numbers, and "every band has the same
//! frame" then never becomes true again. The screen goes black and stays
//! black. Per-band pacing only works if bands may be composited
//! independently, which is exactly what presenting whole frames forbids.
//!
//! The cost is that the slowest band sets the frame rate. That is the honest
//! price of a coherent image, and it is bounded: bands are sized to within
//! one coarse-tile row of each other, so their work is comparable.

/// Per-band scheduling state for one render worker pool.
#[derive(Debug, Clone)]
pub struct RenderBandScheduler {
    /// Frame currently in flight for each band, if any.
    in_flight: Vec<Option<u32>>,
    /// Newest frame each band has delivered a bitmap for.
    delivered: Vec<Option<u32>>,
    /// A band goes unhealthy when its worker fails to spawn or dies. Its
    /// rows then fall to the main thread rather than going missing.
    healthy: Vec<bool>,
    /// Newest frame every healthy band has delivered.
    presented: Option<u32>,
    next_frame_id: u32,
}

impl RenderBandScheduler {
    pub fn new(band_count: usize) -> Self {
        let band_count = band_count.max(1);
        Self {
            in_flight: vec![None; band_count],
            delivered: vec![None; band_count],
            healthy: vec![true; band_count],
            presented: None,
            next_frame_id: 1,
        }
    }

    pub fn band_count(&self) -> usize {
        self.in_flight.len()
    }

    /// Claims the next frame id. Ids are monotonic so a late reply from an
    /// abandoned frame can be recognised and dropped.
    pub fn begin_frame(&mut self) -> u32 {
        let frame_id = self.next_frame_id;
        self.next_frame_id = self.next_frame_id.wrapping_add(1).max(1);
        frame_id
    }

    /// Whether a new frame may be started.
    ///
    /// False while any healthy band is still working. Starting a frame
    /// anyway would put the bands on different frame numbers, and since a
    /// frame is only presented when they all agree, they would never line up
    /// again — the screen would freeze on whatever was last composited.
    pub fn ready_for_new_frame(&self) -> bool {
        (0..self.band_count())
            .filter(|band| self.healthy[*band])
            .all(|band| self.in_flight[band].is_none())
    }

    /// Whether this band takes part in the frame being started.
    ///
    /// Only meaningful once [`Self::ready_for_new_frame`] is true; an
    /// unhealthy band never participates, because the main thread draws it.
    pub fn should_post(&self, band: usize) -> bool {
        self.healthy.get(band).copied().unwrap_or(false) && self.in_flight[band].is_none()
    }

    pub fn mark_posted(&mut self, band: usize, frame_id: u32) {
        if band < self.in_flight.len() {
            self.in_flight[band] = Some(frame_id);
        }
    }

    /// Records a delivered band bitmap. Returns whether it was wanted — a
    /// reply for a frame this band is not working on is stale and its
    /// bitmap should be discarded rather than composited.
    pub fn mark_delivered(&mut self, band: usize, frame_id: u32) -> bool {
        if band >= self.in_flight.len() || self.in_flight[band] != Some(frame_id) {
            return false;
        }
        self.in_flight[band] = None;
        self.delivered[band] = Some(frame_id);
        true
    }

    /// The newest frame every healthy band has delivered, if it has not been
    /// presented already. `None` means keep showing the previous frame.
    pub fn frame_ready_to_present(&mut self) -> Option<u32> {
        let mut ready: Option<u32> = None;
        for band in 0..self.band_count() {
            if !self.healthy[band] {
                continue;
            }
            let delivered = self.delivered[band]?;
            ready = Some(match ready {
                None => delivered,
                Some(current) if current == delivered => delivered,
                // Bands disagree: some are still on an older frame, so no
                // complete frame exists yet.
                Some(_) => return None,
            });
        }
        let ready = ready?;
        if self.presented == Some(ready) {
            return None;
        }
        self.presented = Some(ready);
        Some(ready)
    }

    /// Retires a band whose worker failed. Its in-flight frame is abandoned
    /// so the rest of the pool is not held waiting on a reply that will
    /// never arrive.
    pub fn mark_failed(&mut self, band: usize) {
        if band >= self.healthy.len() {
            return;
        }
        self.healthy[band] = false;
        self.in_flight[band] = None;
        self.delivered[band] = None;
    }

    pub fn is_healthy(&self, band: usize) -> bool {
        self.healthy.get(band).copied().unwrap_or(false)
    }

    /// Bands the main thread must render itself this frame.
    pub fn fallback_bands(&self) -> impl Iterator<Item = usize> + '_ {
        (0..self.band_count()).filter(|band| !self.healthy[*band])
    }

    /// True once no worker is left and the caller should drop to the plain
    /// single-threaded path.
    pub fn all_failed(&self) -> bool {
        self.healthy.iter().all(|healthy| !*healthy)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Starts a frame the way the pool does: only when the pool is ready,
    /// and then to every participating band.
    fn post_all(scheduler: &mut RenderBandScheduler, frame_id: u32) -> Vec<usize> {
        assert!(
            scheduler.ready_for_new_frame(),
            "the pool must not start a frame while a band is still working"
        );
        let posted: Vec<usize> = (0..scheduler.band_count())
            .filter(|band| scheduler.should_post(*band))
            .collect();
        for band in &posted {
            scheduler.mark_posted(*band, frame_id);
        }
        posted
    }

    #[test]
    fn a_frame_is_presented_only_when_every_band_has_delivered_it() {
        let mut scheduler = RenderBandScheduler::new(3);
        let frame = scheduler.begin_frame();
        post_all(&mut scheduler, frame);

        assert!(scheduler.mark_delivered(0, frame));
        assert_eq!(scheduler.frame_ready_to_present(), None, "one band");
        assert!(scheduler.mark_delivered(1, frame));
        assert_eq!(scheduler.frame_ready_to_present(), None, "two bands");
        assert!(scheduler.mark_delivered(2, frame));
        assert_eq!(scheduler.frame_ready_to_present(), Some(frame));
    }

    #[test]
    fn a_complete_frame_is_presented_once() {
        let mut scheduler = RenderBandScheduler::new(2);
        let frame = scheduler.begin_frame();
        post_all(&mut scheduler, frame);
        scheduler.mark_delivered(0, frame);
        scheduler.mark_delivered(1, frame);

        assert_eq!(scheduler.frame_ready_to_present(), Some(frame));
        assert_eq!(
            scheduler.frame_ready_to_present(),
            None,
            "re-compositing an unchanged frame is wasted work"
        );
    }

    #[test]
    fn a_lagging_band_holds_the_frame_rather_than_tearing() {
        let mut scheduler = RenderBandScheduler::new(2);
        let first = scheduler.begin_frame();
        post_all(&mut scheduler, first);
        scheduler.mark_delivered(0, first);
        assert_eq!(
            scheduler.frame_ready_to_present(),
            None,
            "band 1 has not delivered, so nothing whole exists yet"
        );
        scheduler.mark_delivered(1, first);
        assert_eq!(scheduler.frame_ready_to_present(), Some(first));
    }

    #[test]
    fn no_new_frame_starts_while_a_band_is_still_working() {
        let mut scheduler = RenderBandScheduler::new(2);
        let first = scheduler.begin_frame();
        post_all(&mut scheduler, first);

        scheduler.mark_delivered(0, first);
        assert!(
            !scheduler.ready_for_new_frame(),
            "band 1 is still working, so the pool must wait"
        );
        scheduler.mark_delivered(1, first);
        assert!(scheduler.ready_for_new_frame());
    }

    /// The bug that turned the screen black: giving a free band the next
    /// frame while a slower band was still on the previous one put the two
    /// on different frame numbers permanently, and a frame is only shown
    /// when every band agrees — so nothing was ever composited again.
    #[test]
    fn bands_cannot_drift_onto_different_frames_and_stop_presenting() {
        let mut scheduler = RenderBandScheduler::new(2);
        let mut presented = 0;

        // Band 1 always finishes a beat after band 0, forever.
        for _ in 0..32 {
            assert!(scheduler.ready_for_new_frame());
            let frame = scheduler.begin_frame();
            post_all(&mut scheduler, frame);

            scheduler.mark_delivered(0, frame);
            if scheduler.frame_ready_to_present().is_some() {
                presented += 1;
            }
            scheduler.mark_delivered(1, frame);
            if scheduler.frame_ready_to_present().is_some() {
                presented += 1;
            }
        }

        assert_eq!(
            presented, 32,
            "a consistently slower band must cost frame rate, never presentation"
        );
    }

    #[test]
    fn a_stale_reply_is_rejected() {
        let mut scheduler = RenderBandScheduler::new(1);
        let first = scheduler.begin_frame();
        post_all(&mut scheduler, first);
        assert!(scheduler.mark_delivered(0, first));

        let second = scheduler.begin_frame();
        post_all(&mut scheduler, second);
        assert!(
            !scheduler.mark_delivered(0, first),
            "a reply for an abandoned frame must not be composited"
        );
        assert!(scheduler.mark_delivered(0, second));
    }

    #[test]
    fn a_failed_band_stops_blocking_the_rest() {
        let mut scheduler = RenderBandScheduler::new(3);
        let frame = scheduler.begin_frame();
        post_all(&mut scheduler, frame);
        scheduler.mark_delivered(0, frame);
        scheduler.mark_delivered(2, frame);
        assert_eq!(scheduler.frame_ready_to_present(), None);

        // Band 1's worker dies mid-frame. Its rows move to the main thread,
        // and the frame completes on the survivors.
        scheduler.mark_failed(1);
        assert_eq!(scheduler.frame_ready_to_present(), Some(frame));
        assert_eq!(scheduler.fallback_bands().collect::<Vec<_>>(), vec![1]);
        assert!(!scheduler.should_post(1));
        assert!(!scheduler.all_failed());
    }

    #[test]
    fn losing_every_worker_is_detectable() {
        let mut scheduler = RenderBandScheduler::new(2);
        scheduler.mark_failed(0);
        assert!(!scheduler.all_failed());
        scheduler.mark_failed(1);
        assert!(scheduler.all_failed());
        assert_eq!(scheduler.fallback_bands().collect::<Vec<_>>(), vec![0, 1]);
    }

    #[test]
    fn frame_ids_never_reuse_the_sentinel_zero_on_wrap() {
        let mut scheduler = RenderBandScheduler::new(1);
        scheduler.next_frame_id = u32::MAX;
        assert_eq!(scheduler.begin_frame(), u32::MAX);
        assert_eq!(
            scheduler.begin_frame(),
            1,
            "wrapping must skip 0 so it stays available as 'no frame'"
        );
    }

    #[test]
    fn out_of_range_bands_are_ignored_rather_than_panicking() {
        let mut scheduler = RenderBandScheduler::new(1);
        assert!(!scheduler.should_post(9));
        assert!(!scheduler.mark_delivered(9, 1));
        scheduler.mark_posted(9, 1);
        scheduler.mark_failed(9);
        assert!(scheduler.is_healthy(0));
    }
}
