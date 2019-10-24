// Copyright (C) 2019  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

use ii_logging::macros::*;

use crate::node;

use ii_stats::WindowedTimeMean;

use futures::lock::{Mutex, MutexGuard};
use ii_async_compat::{futures, tokio};
use tokio::timer::delay_for;

use std::time;

use lazy_static::lazy_static;

lazy_static! {
    static ref DEFAULT_TIME_MEAN_INTERVALS: Vec<time::Duration> = vec![
        time::Duration::from_secs(5),
        time::Duration::from_secs(1 * 60),
        time::Duration::from_secs(5 * 60),
        time::Duration::from_secs(15 * 60),
        time::Duration::from_secs(24 * 60 * 60),
    ];
}

struct MeterInner {
    /// All shares measured from the beginning of mining
    shares: ii_bitcoin::Shares,
    /// Approximate arithmetic mean of hashes within given time intervals (in kH/time)
    time_means: Vec<WindowedTimeMean>,
}

#[derive(Debug)]
pub struct Meter {
    inner: Mutex<MeterInner>,
}

impl Meter {
    pub fn new(intervals: &Vec<time::Duration>) -> Self {
        Self {
            inner: Mutex::new(MeterInner {
                shares: Default::default(),
                time_means: intervals
                    .iter()
                    .map(|&interval| WindowedTimeMean::new(interval))
                    .collect(),
            }),
        }
    }

    pub async fn shares(&self) -> SharesGuard<'_> {
        SharesGuard(self.inner.lock().await)
    }

    pub async fn time_means(&self) -> TimeMeansGuard<'_> {
        TimeMeansGuard(self.inner.lock().await)
    }

    pub(crate) async fn account_solution(&self, target: &ii_bitcoin::Target, time: time::Instant) {
        let mut meter = self.inner.lock().await;
        let kilo_hashes = ii_bitcoin::Shares::new(target).into_kilo_hashes();

        meter.shares.account_solution(target);
        for time_mean in &mut meter.time_means {
            time_mean.insert(kilo_hashes, time);
        }
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self::new(DEFAULT_TIME_MEAN_INTERVALS.as_ref())
    }
}

pub struct SharesGuard<'a>(MutexGuard<'a, MeterInner>);

impl<'a> std::ops::Deref for SharesGuard<'a> {
    type Target = ii_bitcoin::Shares;

    fn deref(&self) -> &Self::Target {
        &self.0.shares
    }
}

pub struct TimeMeansGuard<'a>(MutexGuard<'a, MeterInner>);

impl<'a> std::ops::Deref for TimeMeansGuard<'a> {
    type Target = Vec<WindowedTimeMean>;

    fn deref(&self) -> &Self::Target {
        &self.0.time_means
    }
}

#[derive(Debug)]
pub struct Mining {
    pub start_time: time::Instant,
    pub accepted: Meter,
    pub rejected: Meter,
    pub backend_error: Meter,
}

impl Mining {
    pub fn new(start_time: time::Instant, intervals: &Vec<time::Duration>) -> Self {
        Self {
            start_time,
            accepted: Meter::new(&intervals),
            rejected: Meter::new(&intervals),
            backend_error: Meter::new(&intervals),
        }
    }
}

impl Default for Mining {
    fn default() -> Self {
        Self::new(time::Instant::now(), DEFAULT_TIME_MEAN_INTERVALS.as_ref())
    }
}

pub(crate) async fn account_accepted(
    path: &node::Path,
    solution_target: &ii_bitcoin::Target,
    time: time::Instant,
) {
    for node in path {
        node.mining_stats()
            .accepted
            .account_solution(solution_target, time)
            .await;
    }
}

pub async fn mining_task(node: node::DynInfo, interval: time::Duration) {
    loop {
        delay_for(time::Duration::from_secs(1)).await;

        let time_means = node.mining_stats().accepted.time_means().await;
        let time_mean = time_means
            .iter()
            .find(|time_mean| time_mean.interval() == interval)
            .expect("cannot find given time interval");

        info!(
            "Hash rate @ pool difficulty: {:.2} GH/{}s",
            time_mean.measure(time::Instant::now()) * 1e-6,
            time_mean.interval().as_secs()
        );
    }
}
