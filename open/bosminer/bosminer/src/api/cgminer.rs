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

//! This module implements CGMiner compatible API server to control bOSminer and to extract
//! statistics from it.

use crate::hub;
use crate::node::{self, Stats as _, WorkSolverStats as _};
use crate::stats::{self, UnixTime as _};
use crate::version;

use ii_cgminer_api::support::ValueExt as _;
use ii_cgminer_api::{command, json, response};

use std::future::Future;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time;

use stats::TIME_MEAN_INTERVAL_15M as INTERVAL_15M;
use stats::TIME_MEAN_INTERVAL_1M as INTERVAL_1M;
use stats::TIME_MEAN_INTERVAL_5M as INTERVAL_5M;
use stats::TIME_MEAN_INTERVAL_5S as INTERVAL_5S;

/// Miner signature where `CGMiner` text is used to be
const SIGNATURE: &str = "bOSminer";

/// Default interval used for computation of default rolling average.
const DEFAULT_LOG_INTERVAL: u32 = 5;

struct Handler {
    core: Arc<hub::Core>,
}

impl Handler {
    pub fn new(core: Arc<hub::Core>) -> Self {
        Self { core }
    }

    async fn collect_data<C, F, T, U, V>(&self, container: C, base_idx: usize, f: F) -> Vec<T>
    where
        C: Future<Output = Vec<Arc<U>>>,
        F: Fn(usize, Arc<U>) -> V,
        U: ?Sized,
        V: Future<Output = T>,
    {
        let mut list = vec![];
        for (idx, item) in container.await.iter().enumerate() {
            list.push(f(base_idx + idx, item.clone()).await);
        }
        list
    }

    async fn get_pool_status(idx: usize, client: &Arc<dyn node::Client>) -> response::Pool {
        let descriptor = client.descriptor().expect("BUG: missing client descriptor");
        let last_job = client.get_last_job().await;

        let client_stats = client.client_stats();
        let valid_jobs = client_stats.valid_jobs().take_snapshot();
        let invalid_jobs = client_stats.invalid_jobs().take_snapshot();
        let generated_work = client_stats.generated_work().take_snapshot();
        let accepted = client_stats.accepted().take_snapshot().await;
        let rejected = client_stats.rejected().take_snapshot().await;
        let stale = client_stats.stale().take_snapshot().await;
        let last_share = client_stats.last_share().take_snapshot().await;
        let valid_backend_diff = client_stats.valid_backend_diff().take_snapshot().await;
        let best_share = client_stats.best_share().take_snapshot();

        let last_share_time = last_share
            .as_ref()
            .map_or(0, |share| share.time.get_unix_time().unwrap_or_default());
        let last_share_difficulty = last_share.map_or(0.0, |share| share.difficulty as f64);

        let pool_accepted_shares = accepted.shares.as_f64();
        let pool_rejected_shares = rejected.shares.as_f64();
        let pool_stale_shares = stale.shares.as_f64();
        let pool_total_shares = pool_accepted_shares + pool_rejected_shares + pool_stale_shares;
        let pool_rejected_ratio = if pool_total_shares != 0.0 {
            pool_rejected_shares / pool_total_shares * 100.0
        } else {
            0.0
        };
        let pool_stale_ratio = if pool_total_shares != 0.0 {
            pool_stale_shares / pool_total_shares * 100.0
        } else {
            0.0
        };

        let last_diff = last_job
            .as_ref()
            .map(|job| job.target().get_difficulty() as f64)
            .unwrap_or(0.0);
        let current_block_version = last_job.map(|job| job.version()).unwrap_or_default();

        response::Pool {
            idx: idx as i32,
            url: descriptor.url.clone(),
            // TODO: get actual status from client
            status: response::PoolStatus::Alive,
            // TODO: get actual value from client
            priority: 0,
            // TODO: get actual value from client
            quota: 1,
            // TODO: get actual value from client?
            long_poll: response::Bool::N,
            getworks: *valid_jobs as u32,
            accepted: accepted.solutions,
            rejected: rejected.solutions,
            works: *generated_work as i32,
            // TODO: bOSminer does not account this information
            discarded: 0,
            stale: stale.solutions as u32,
            // TODO: account failures
            get_failures: 0,
            // TODO: account remote failures
            remote_failures: 0,
            user: descriptor.user.clone(),
            last_share_time,
            diff1_shares: valid_backend_diff.solutions,
            proxy_type: "".to_string(),
            proxy: "".to_string(),
            difficulty_accepted: pool_accepted_shares,
            difficulty_rejected: pool_rejected_shares,
            difficulty_stale: pool_stale_shares,
            last_share_difficulty,
            work_difficulty: last_diff,
            has_stratum: true,
            // TODO: get actual value from client
            stratum_active: true,
            // TODO: stratum_url shows url without stratum prefix
            stratum_url: descriptor.url.clone(),
            stratum_difficulty: last_diff,
            // TODO: get actual value from client (Asic Boost)
            has_vmask: true,
            has_gbt: false,
            best_share: best_share.map(|inner| *inner).unwrap_or_default() as u64,
            pool_rejected_ratio,
            pool_stale_ratio,
            bad_work: *invalid_jobs as u64,
            // TODO: bOSminer does not have coinbase for Stratum V2
            current_block_height: 0,
            current_block_version,
        }
    }

    async fn collect_pool_statuses(&self) -> Vec<response::Pool> {
        self.collect_data(self.core.get_clients(), 0, |idx, client| {
            async move { Self::get_pool_status(idx, &client).await }
        })
        .await
    }

    async fn get_asc_status(idx: usize, work_solver: &Arc<dyn node::WorkSolver>) -> response::Asc {
        let mining_stats = work_solver.mining_stats();
        let work_solver_stats = work_solver.work_solver_stats();
        let last_work_time = work_solver_stats.last_work_time().take_snapshot().await;
        let last_share = mining_stats.last_share().take_snapshot().await;
        let valid_job_diff = mining_stats.valid_job_diff().take_snapshot().await;
        let valid_backend_diff = mining_stats.valid_backend_diff().take_snapshot().await;
        let error_backend_diff = mining_stats.error_backend_diff().take_snapshot().await;

        let now = time::Instant::now();
        let elapsed = now.duration_since(*mining_stats.start_time());

        let last_work_time =
            last_work_time.map_or(0, |time| time.get_unix_time().unwrap_or_default());
        let last_share_time = last_share
            .as_ref()
            .map_or(0, |share| share.time.get_unix_time().unwrap_or_default());
        let last_share_difficulty = last_share.map_or(0.0, |share| share.difficulty as f64);

        let total_mega_hashes = valid_job_diff.shares.into_mega_hashes().into_f64();
        let backend_valid_solutions = valid_backend_diff.solutions;
        let backend_error_solutions = error_backend_diff.solutions;
        let backend_all_solutions = backend_error_solutions + backend_valid_solutions;
        let backend_error_ratio = if backend_all_solutions != 0 {
            backend_error_solutions as f64 / backend_all_solutions as f64 * 100.0
        } else {
            0.0
        };

        response::Asc {
            idx: idx as i32,
            // TODO: get actual ASIC name from work solver
            name: "".to_string(),
            // TODO: get idx from work solver (it can represent real index of hash chain)
            id: idx as i32,
            // TODO: get actual state from work solver
            enabled: response::Bool::Y,
            // TODO: get actual status from work solver
            status: response::AscStatus::Alive,
            // TODO: get actual temperature from work solver?
            temperature: 0.0,
            mhs_av: total_mega_hashes / elapsed.as_secs_f64(),
            mhs_5s: valid_job_diff.to_mega_hashes(*INTERVAL_5S, now).into_f64(),
            mhs_1m: valid_job_diff.to_mega_hashes(*INTERVAL_1M, now).into_f64(),
            mhs_5m: valid_job_diff.to_mega_hashes(*INTERVAL_5M, now).into_f64(),
            mhs_15m: valid_job_diff.to_mega_hashes(*INTERVAL_15M, now).into_f64(),
            // TODO: bOSminer does not account this information
            accepted: 0,
            // TODO: bOSminer does not account this information
            rejected: 0,
            hardware_errors: backend_error_solutions as i32,
            // TODO: bOSminer does not account accepted
            utility: 0.0,
            // TODO: bOSminer does not account accepted
            last_share_pool: -1,
            last_share_time,
            total_mega_hashes,
            diff1_work: backend_valid_solutions,
            // TODO: bOSminer does not account accepted
            difficulty_accepted: 0.0,
            // TODO: bOSminer does not account rejected
            difficulty_rejected: 0.0,
            last_share_difficulty,
            last_valid_work: last_work_time,
            device_hardware_ratio: backend_error_ratio,
            // TODO: bOSminer does not account rejected
            device_rejected_ratio: 0.0,
            device_elapsed: elapsed.as_secs(),
        }
    }

    async fn collect_asc_statuses(&self) -> Vec<response::Asc> {
        self.collect_data(self.core.get_work_solvers(), 0, |idx, work_solver| {
            async move { Self::get_asc_status(idx, &work_solver).await }
        })
        .await
    }

    async fn get_dev_detail(
        idx: usize,
        _work_solver: &Arc<dyn node::WorkSolver>,
    ) -> response::DevDetail {
        response::DevDetail {
            idx: idx as i32,
            // TODO: get actual ASIC name from work solver
            name: "".to_string(),
            // TODO: get idx from work solver (it can represent real index of hash chain)
            id: idx as i32,
            // TODO: get remaining information from work solver
            driver: "".to_string(),
            kernel: "".to_string(),
            model: "".to_string(),
            device_path: "".to_string(),
        }
    }

    async fn collect_dev_details(&self) -> Vec<response::DevDetail> {
        self.collect_data(self.core.get_work_solvers(), 0, |idx, work_solver| {
            async move { Self::get_dev_detail(idx, &work_solver).await }
        })
        .await
    }

    async fn get_pool_stats(idx: usize, _client: &Arc<dyn node::Client>) -> response::PoolStats {
        response::PoolStats {
            header: response::StatsHeader {
                idx: idx as i32,
                id: "".to_string(),
                elapsed: 0,
                calls: 0,
                wait: 0.0,
                max: 0.0,
                min: 0.0,
            },
            pool_calls: 0,
            pool_attempts: 0,
            pool_wait: 0.0,
            pool_max: 0.0,
            pool_min: 0.0,
            pool_av: 0.0,
            work_had_roll_time: false,
            work_can_roll: false,
            work_had_expire: false,
            work_roll_time: 0,
            work_diff: 0.0,
            min_diff: 0.0,
            max_diff: 0.0,
            min_diff_count: 0,
            max_diff_count: 0,
            times_sent: 0,
            bytes_sent: 0,
            times_recv: 0,
            bytes_recv: 0,
            net_bytes_sent: 0,
            net_bytes_recv: 0,
        }
    }

    async fn collect_pool_stats(&self, base_idx: usize) -> Vec<response::PoolStats> {
        self.collect_data(self.core.get_clients(), base_idx, |idx, client| {
            async move { Self::get_pool_stats(idx, &client).await }
        })
        .await
    }

    async fn get_asc_stats(
        idx: usize,
        _work_solver: &Arc<dyn node::WorkSolver>,
    ) -> response::AscStats {
        response::AscStats {
            header: response::StatsHeader {
                idx: idx as i32,
                id: "".to_string(),
                elapsed: 0,
                calls: 0,
                wait: 0.0,
                max: 0.0,
                min: 0.0,
            },
        }
    }

    async fn collect_asc_stats(&self, base_idx: usize) -> Vec<response::AscStats> {
        self.collect_data(
            self.core.get_work_solvers(),
            base_idx,
            |idx, work_solver| async move { Self::get_asc_stats(idx, &work_solver).await },
        )
        .await
    }
}

#[async_trait::async_trait]
impl command::Handler for Handler {
    async fn handle_pools(&self) -> command::Result<response::Pools> {
        Ok(response::Pools {
            list: self.collect_pool_statuses().await,
        })
    }

    async fn handle_devs(&self) -> command::Result<response::Devs> {
        Ok(response::Devs {
            list: self.collect_asc_statuses().await,
        })
    }

    async fn handle_edevs(&self) -> command::Result<response::Devs> {
        self.handle_devs().await
    }

    async fn handle_summary(&self) -> command::Result<response::Summary> {
        let frontend = self.core.frontend.clone();

        let mining_stats = frontend.mining_stats();
        let work_solver_stats = frontend.work_solver_stats();
        let last_work_time = work_solver_stats.last_work_time().take_snapshot().await;
        let generated_work = work_solver_stats.generated_work().take_snapshot();
        let valid_network_diff = mining_stats.valid_network_diff().take_snapshot().await;
        let valid_job_diff = mining_stats.valid_job_diff().take_snapshot().await;
        let valid_backend_diff = mining_stats.valid_backend_diff().take_snapshot().await;
        let error_backend_diff = mining_stats.error_backend_diff().take_snapshot().await;
        let best_share = mining_stats.best_share().take_snapshot();

        let now = time::Instant::now();
        let elapsed = now.duration_since(*mining_stats.start_time());

        let last_work_time =
            last_work_time.map_or(0, |time| time.get_unix_time().unwrap_or_default());

        let total_mega_hashes = valid_job_diff.shares.into_mega_hashes().into_f64();
        let network_valid_solutions = valid_network_diff.solutions;
        let backend_valid_solutions = valid_backend_diff.solutions;
        let backend_error_solutions = error_backend_diff.solutions;
        let backend_all_solutions = backend_error_solutions + backend_valid_solutions;
        let backend_error_ratio = if backend_all_solutions != 0 {
            backend_error_solutions as f64 / backend_all_solutions as f64
        } else {
            0.0
        } * 100.0;
        let work_utility = valid_backend_diff.shares.to_sharerate(elapsed) * 60.0;

        Ok(response::Summary {
            elapsed: elapsed.as_secs(),
            mhs_av: total_mega_hashes / elapsed.as_secs_f64(),
            mhs_5s: valid_job_diff.to_mega_hashes(*INTERVAL_5S, now).into_f64(),
            mhs_1m: valid_job_diff.to_mega_hashes(*INTERVAL_1M, now).into_f64(),
            mhs_5m: valid_job_diff.to_mega_hashes(*INTERVAL_5M, now).into_f64(),
            mhs_15m: valid_job_diff.to_mega_hashes(*INTERVAL_15M, now).into_f64(),
            found_blocks: network_valid_solutions as u32,
            // TODO: bOSminer does not account this information
            getworks: 0,
            // TODO: bOSminer does not account this information
            accepted: 0,
            // TODO: bOSminer does not account this information
            rejected: 0,
            hardware_errors: backend_error_solutions as i32,
            // TODO: bOSminer does not account accepted
            utility: 0.0,
            // TODO: bOSminer does not account accepted
            discarded: 0,
            // TODO: bOSminer does not account this information
            stale: 0,
            // TODO: bOSminer does not account this information
            get_failures: 0,
            local_work: *generated_work as u32,
            // TODO: bOSminer does not account this information
            remote_failures: 0,
            // TODO: bOSminer does not account this information
            network_blocks: 0,
            total_mega_hashes,
            work_utility,
            // TODO: bOSminer does not account this information
            difficulty_accepted: 0.0,
            // TODO: bOSminer does not account this information
            difficulty_rejected: 0.0,
            // TODO: bOSminer does not account this information
            difficulty_stale: 0.0,
            best_share: best_share.map(|inner| *inner).unwrap_or_default() as u64,
            device_hardware_ratio: backend_error_ratio,
            // TODO: bosminer does not account rejected
            device_rejected_ratio: 0.0,
            // TODO: bosminer does not account rejected
            pool_rejected_ratio: 0.0,
            // TODO: bosminer does not account stale
            pool_stale_ratio: 0.0,
            last_getwork: last_work_time,
        })
    }

    async fn handle_config(&self) -> command::Result<response::Config> {
        Ok(response::Config {
            asc_count: self.core.get_work_solvers().await.len() as i32,
            pga_count: 0,
            pool_count: self.core.get_clients().await.len() as i32,
            // TODO: get actual multi-pool strategy
            strategy: response::MultipoolStrategy::Failover,
            log_interval: DEFAULT_LOG_INTERVAL as i32,
            device_code: String::new(),
            // TODO: detect underlying operation system
            os: "Braiins OS".to_string(),
            hotplug: "None".to_string(),
        })
    }

    async fn handle_dev_details(&self) -> command::Result<response::DevDetails> {
        Ok(response::DevDetails {
            list: self.collect_dev_details().await,
        })
    }

    async fn handle_stats(&self) -> command::Result<response::Stats> {
        let asc_stats = self.collect_asc_stats(0).await;
        let pool_stats = self.collect_pool_stats(asc_stats.len()).await;
        Ok(response::Stats {
            asc_stats,
            pool_stats,
        })
    }

    async fn handle_estats(&self) -> command::Result<response::Stats> {
        Ok(response::Stats {
            asc_stats: self.collect_asc_stats(0).await,
            pool_stats: vec![],
        })
    }

    async fn handle_coin(&self) -> command::Result<response::Coin> {
        Ok(response::Coin {
            hash_method: "sha256".to_string(),
            // TODO: get actual value from clients
            current_block_time: 0.0,
            // TODO: get actual value from clients
            current_block_hash: "".to_string(),
            lp: true,
            // TODO: get actual value from clients
            network_difficulty: 0.0,
        })
    }

    async fn handle_asc_count(&self) -> command::Result<response::AscCount> {
        Ok(response::AscCount {
            count: self.core.get_work_solvers().await.len() as i32,
        })
    }

    async fn handle_asc(&self, parameter: Option<&json::Value>) -> command::Result<response::Asc> {
        let idx = parameter
            .expect("BUG: missing ASC parameter")
            .to_i32()
            .expect("BUG: invalid ASC parameter type");

        let work_solvers = self.core.get_work_solvers().await;
        let work_solver = work_solvers.get(idx as usize).cloned();

        match work_solver {
            Some(work_solver) => Ok(Self::get_asc_status(idx as usize, &work_solver).await),
            None => {
                Err(response::ErrorCode::InvalidAscId(idx, work_solvers.len() as i32 - 1).into())
            }
        }
    }

    async fn handle_lcd(&self) -> command::Result<response::Lcd> {
        // TODO: implement response
        Ok(response::Lcd {
            elapsed: 0,
            ghs_av: 0.0,
            ghs_5m: 0.0,
            ghs_5s: 0.0,
            temperature: 0.0,
            last_share_difficulty: 0.0,
            last_share_time: 0,
            best_share: 0,
            last_valid_work: 0,
            found_blocks: 0,
            current_pool: "".to_string(),
            user: "".to_string(),
        })
    }
}

pub async fn run(
    core: Arc<hub::Core>,
    listen_addr: SocketAddr,
    custom_commands: Option<command::Map>,
) {
    let handler = Handler::new(core);
    let command_receiver = command::Receiver::new(
        handler,
        SIGNATURE.to_string(),
        version::STRING.to_string(),
        custom_commands,
    );

    ii_cgminer_api::run(command_receiver, listen_addr)
        .await
        .unwrap();
}
