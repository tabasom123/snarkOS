// Copyright (C) 2019-2023 Aleo Systems Inc.
// This file is part of the snarkOS library.

// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at:
// http://www.apache.org/licenses/LICENSE-2.0

// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

// Avoid a false positive from clippy:
// https://github.com/rust-lang/rust-clippy/issues/6446
#![allow(clippy::await_holding_lock)]

use snarkvm::prelude::{
    block::Block,
    store::{cow_to_copied, ConsensusStorage},
    Deserialize,
    DeserializeOwned,
    Ledger,
    Network,
    Serialize,
};

use anyhow::{anyhow, bail, Result};
use colored::Colorize;
use core::ops::Range;
use parking_lot::Mutex;
use reqwest::Client;
use std::{
    cmp,
    sync::{
        atomic::{AtomicBool, AtomicU32, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

/// The number of blocks per file.
const BLOCKS_PER_FILE: u32 = 50;
/// The desired number of concurrent requests to the CDN.
const CONCURRENT_REQUESTS: u32 = 16;
/// Maximum number of pending sync blocks.
const MAXIMUM_PENDING_BLOCKS: u32 = BLOCKS_PER_FILE * CONCURRENT_REQUESTS * 2;
/// The supported network.
const NETWORK_ID: u16 = 3;

/// Loads blocks from a CDN into the ledger.
///
/// On success, this function returns the completed block height.
/// On failure, this function returns the last successful block height (if any), along with the error.
pub async fn sync_ledger_with_cdn<N: Network, C: ConsensusStorage<N>>(
    base_url: &str,
    ledger: Ledger<N, C>,
    shutdown: Arc<AtomicBool>,
) -> Result<u32, (u32, anyhow::Error)> {
    // Fetch the node height.
    let start_height = ledger.latest_height() + 1;
    // Load the blocks from the CDN into the ledger.
    let ledger_clone = ledger.clone();
    let result = load_blocks(base_url, start_height, None, shutdown, move |block: Block<N>| {
        ledger_clone.advance_to_next_block(&block)
    })
    .await;

    // TODO (howardwu): Find a way to resolve integrity failures.
    // If the sync failed, check the integrity of the ledger.
    if let Err((completed_height, error)) = &result {
        warn!("{error}");

        // If the sync made any progress, then check the integrity of the ledger.
        if *completed_height != start_height {
            debug!("Synced the ledger up to block {completed_height}");

            // Retrieve the latest height, according to the ledger.
            let node_height = cow_to_copied!(ledger.vm().block_store().heights().max().unwrap_or_default());
            // Check the integrity of the latest height.
            if &node_height != completed_height {
                return Err((*completed_height, anyhow!("The ledger height does not match the last sync height")));
            }

            // Fetch the latest block from the ledger.
            if let Err(err) = ledger.get_block(node_height) {
                return Err((*completed_height, err));
            }
        }

        Ok(*completed_height)
    } else {
        result
    }
}

/// Loads blocks from a CDN and process them with the given function.
///
/// On success, this function returns the completed block height.
/// On failure, this function returns the last successful block height (if any), along with the error.
pub async fn load_blocks<N: Network>(
    base_url: &str,
    start_height: u32,
    end_height: Option<u32>,
    shutdown: Arc<AtomicBool>,
    process: impl FnMut(Block<N>) -> Result<()> + Clone + Send + Sync + 'static,
) -> Result<u32, (u32, anyhow::Error)> {
    // If the network is not supported, return.
    if N::ID != NETWORK_ID {
        return Err((start_height, anyhow!("The network ({}) is not supported", N::ID)));
    }

    // Fetch the CDN height.
    let cdn_height = match cdn_height::<BLOCKS_PER_FILE>(base_url).await {
        Ok(cdn_height) => cdn_height,
        Err(error) => return Err((start_height, error)),
    };
    // If the CDN height is less than the start height, return.
    if cdn_height < start_height {
        return Err((
            start_height,
            anyhow!("The given start height ({start_height}) must be less than the CDN height ({cdn_height})"),
        ));
    }

    // If the end height is not specified, set it to the CDN height.
    // If the end height is greater than the CDN height, set the end height to the CDN height.
    let end_height = cmp::min(end_height.unwrap_or(cdn_height), cdn_height);
    // If the end height is less than the start height, return.
    if end_height < start_height {
        return Err((
            start_height,
            anyhow!("The given end height ({end_height}) must be less than the start height ({start_height})"),
        ));
    }

    // Compute the CDN start height rounded down to the nearest multiple.
    let cdn_start = start_height - (start_height % BLOCKS_PER_FILE);
    // Set the CDN end height to the given end height.
    let cdn_end = end_height;
    // Construct the CDN range.
    let cdn_range = cdn_start..cdn_end;
    // If the CDN range is empty, return.
    if cdn_range.is_empty() {
        return Ok(cdn_end);
    }

    // A collection of dowloaded blocks pending insertion into the ledger.
    let pending_blocks: Arc<Mutex<Vec<Block<N>>>> = Default::default();

    // Start a timer.
    let timer = Instant::now();

    // Create a Client to maintain a connection pool throughout the sync.
    let client = match Client::builder().build() {
        Ok(client) => client,
        Err(error) => {
            return Err((start_height.saturating_sub(1), anyhow!("Failed to create a CDN request client: {error}")));
        }
    };

    // Spawn a task responsible for concurrent downloads.
    let pending_blocks_clone = pending_blocks.clone();
    let base_url = base_url.to_owned();
    tokio::spawn(async move {
        // Keep track of the number of concurrent requests.
        let active_requests: Arc<AtomicU32> = Default::default();

        let mut start = cdn_start;
        while start < cdn_end - 1 {
            // Avoid collecting too many blocks in order to restrict memory use.
            let num_pending_blocks = pending_blocks_clone.lock().len();
            if num_pending_blocks >= MAXIMUM_PENDING_BLOCKS as usize {
                debug!("Maximum number of pending blocks reached ({num_pending_blocks}), waiting...");
                tokio::time::sleep(Duration::from_secs(5)).await;
                continue;
            }

            // Stop looping once we have enough pending blocks and ongoing requests.
            let active_request_count = active_requests.load(Ordering::Relaxed);
            if start + num_pending_blocks as u32 + active_request_count * BLOCKS_PER_FILE >= cdn_end - 1 {
                debug!("Reached the end of the syncing range; stopping CDN requests");
                break;
            }

            // The number of concurrent requests is maintained at CONCURRENT_REQUESTS, unless the maximum
            // number of pending blocks may be breached.
            let num_requests =
                cmp::min(CONCURRENT_REQUESTS, (MAXIMUM_PENDING_BLOCKS - num_pending_blocks as u32) / BLOCKS_PER_FILE)
                    .saturating_sub(active_request_count);

            // Spawn concurrent requests for bundles of blocks.
            for i in 0..num_requests {
                let start = start + i * BLOCKS_PER_FILE;
                let end = start + BLOCKS_PER_FILE;

                // If this request would breach the upper limit, stop downloading.
                if end > cdn_end + BLOCKS_PER_FILE {
                    debug!("Reached the end of the syncing range; stopping CDN requests");
                    break;
                }

                let client_clone = client.clone();
                let base_url_clone = base_url.clone();
                let pending_blocks_clone = pending_blocks_clone.clone();
                let active_requests_clone = active_requests.clone();
                tokio::spawn(async move {
                    // Increment the number of active requests.
                    active_requests_clone.fetch_add(1, Ordering::Relaxed);

                    let ctx = format!("blocks {start} to {end}");
                    debug!("Requesting {ctx} (of {cdn_end})");

                    // Prepare the URL.
                    let blocks_url = format!("{base_url_clone}/{start}.{end}.blocks");
                    let ctx = format!("blocks {start} to {end}");
                    // Download blocks, retrying on failure.
                    let mut attempts = 0;
                    let request_time = Instant::now();

                    loop {
                        // Fetch the blocks.
                        match cdn_get(client_clone.clone(), &blocks_url, &ctx).await {
                            Ok::<Vec<Block<N>>, _>(blocks) => {
                                // Keep the collection of pending blocks sorted by the height.
                                let mut pending_blocks = pending_blocks_clone.lock();
                                for block in blocks {
                                    match pending_blocks.binary_search_by_key(&block.height(), |b| b.height()) {
                                        Ok(_idx) => warn!("Duplicate pending block at height {}", block.height()),
                                        Err(idx) => pending_blocks.insert(idx, block),
                                    }
                                }
                                debug!(
                                    "Received {ctx} in {:.2?} ({} queued for insertion)",
                                    request_time.elapsed(),
                                    pending_blocks.len()
                                );
                                break;
                            }
                            Err(error) => {
                                // Increment the attempt counter, and wait with a linear backoff.
                                attempts += 1;
                                tokio::time::sleep(Duration::from_secs(attempts)).await;
                                warn!("Failed to request {ctx} - {error}; retrying ({attempts} attempt(s) so far)");
                            }
                        }
                    }

                    // Decrement the number of active requests.
                    active_requests_clone.fetch_sub(1, Ordering::Relaxed);
                });
            }

            // Increase the starting block height for the subsequent requests.
            start += BLOCKS_PER_FILE * num_requests;

            // A short sleep in order to allow some block processing to happen in the meantime.
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
    });

    // A loop for inserting the pending blocks into the ledger.
    let mut current_height = start_height.saturating_sub(1);
    while current_height < end_height - 1 {
        let mut candidate_blocks = pending_blocks.lock();

        // Obtain the height of the nearest pending block.
        let Some(next_height) = candidate_blocks.first().map(|b| b.height()) else {
            debug!("No pending blocks yet");
            drop(candidate_blocks);
            tokio::time::sleep(Duration::from_secs(3)).await;
            continue;
        };

        // Wait if the nearest pending block is not the next one that can be inserted.
        if next_height > current_height + 1 {
            // There is a gap in pending blocks, we need to wait.
            debug!(
                "First candidate's height {} > {}; {} pending",
                next_height,
                current_height + 1,
                candidate_blocks.len()
            );
            drop(candidate_blocks);
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        }

        // Obtain the first BLOCKS_PER_FILE applicable blocks.
        let retained_blocks = candidate_blocks.split_off(BLOCKS_PER_FILE as usize);
        let next_blocks = std::mem::replace(&mut *candidate_blocks, retained_blocks);
        drop(candidate_blocks);

        // Attempt to advance the ledger using the CDN block bundle.
        for block in next_blocks {
            // If the Ctrl-C handler registered the signal, stop the sync.
            if shutdown.load(Ordering::Relaxed) {
                info!("Stopping block sync (at {}) - the node is shutting down", block.height());
                // Note: Calling 'exit' from here is not ideal, but the CDN sync happens before
                // the node is even initialized, so it doesn't result in any other
                // functionalities being shut down abruptly.
                std::process::exit(0);
            }

            // Due to the CDN serving bundles of specific size, we may receive some redundant blocks.
            if block.height() < start_height || current_height >= end_height - 1 {
                debug!("Skipping block {}", block.height());
                continue;
            }

            // Insert the block into the ledger.
            let mut process_clone = process.clone();
            let result = tokio::task::spawn_blocking(move || process_clone(block)).await;

            // Abort syncing on failure.
            match result {
                Ok(Ok(_)) => {}
                Ok(Err(err)) => {
                    return Err((current_height, err));
                }
                Err(err) => {
                    return Err((current_height, err.into()));
                }
            }

            current_height += 1;

            // Log the progress.
            log_progress::<BLOCKS_PER_FILE>(timer, current_height, &cdn_range, "block");
        }
    }

    Ok(current_height)
}

/// Retrieves the CDN height with the given base URL.
///
/// Note: This function decrements the tip by a few blocks, to ensure the
/// tip is not on a block that is not yet available on the CDN.
async fn cdn_height<const BLOCKS_PER_FILE: u32>(base_url: &str) -> Result<u32> {
    // A representation of the 'latest.json' file object.
    #[derive(Deserialize, Serialize, Debug)]
    struct LatestState {
        exclusive_height: u32,
        inclusive_height: u32,
        hash: String,
    }
    // Create a request client.
    let client = match reqwest::Client::builder().build() {
        Ok(client) => client,
        Err(error) => bail!("Failed to create a CDN request client: {error}"),
    };
    // Prepare the URL.
    let latest_json_url = format!("{base_url}/latest.json");
    // Send the request.
    let response = match client.get(latest_json_url).send().await {
        Ok(response) => response,
        Err(error) => bail!("Failed to fetch the CDN height: {error}"),
    };
    // Parse the response.
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => bail!("Failed to parse the CDN height response: {error}"),
    };
    // Parse the bytes for the string.
    let latest_state_string = match bincode::deserialize::<String>(&bytes) {
        Ok(string) => string,
        Err(error) => bail!("Failed to deserialize the CDN height response: {error}"),
    };
    // Parse the string for the tip.
    let tip = match serde_json::from_str::<LatestState>(&latest_state_string) {
        Ok(latest) => latest.exclusive_height,
        Err(error) => bail!("Failed to extract the CDN height response: {error}"),
    };
    // Decrement the tip by a few blocks to ensure the CDN is caught up.
    let tip = tip.saturating_sub(10);
    // Adjust the tip to the closest subsequent multiple of BLOCKS_PER_FILE.
    Ok(tip - (tip % BLOCKS_PER_FILE) + BLOCKS_PER_FILE)
}

/// Retrieves the objects from the CDN with the given URL.
async fn cdn_get<T: 'static + DeserializeOwned + Send>(client: Client, url: &str, ctx: &str) -> Result<T> {
    // Fetch the bytes from the given URL.
    let response = match client.get(url).send().await {
        Ok(response) => response,
        Err(error) => bail!("Failed to fetch {ctx}: {error}"),
    };
    // Parse the response.
    let bytes = match response.bytes().await {
        Ok(bytes) => bytes,
        Err(error) => bail!("Failed to parse {ctx}: {error}"),
    };
    // Parse the objects.
    match tokio::task::spawn_blocking(move || bincode::deserialize::<T>(&bytes)).await {
        Ok(Ok(objects)) => Ok(objects),
        Ok(Err(error)) => bail!("Failed to deserialize {ctx}: {error}"),
        Err(error) => bail!("Failed to join task for {ctx}: {error}"),
    }
}

/// Logs the progress of the sync.
fn log_progress<const OBJECTS_PER_FILE: u32>(
    timer: Instant,
    current_index: u32,
    cdn_range: &Range<u32>,
    object_name: &str,
) {
    // Prepare the CDN start and end heights.
    let cdn_start = cdn_range.start;
    // Subtract 1, as the end of the range is exclusive.
    let cdn_end = cdn_range.end - 1;
    // Compute the percentage completed.
    let percentage = current_index * 100 / cdn_end;
    // Compute the number of files processed so far.
    let num_files_done = 1 + (current_index - cdn_start) / OBJECTS_PER_FILE;
    // Compute the number of files remaining.
    let num_files_remaining = 1 + (cdn_end.saturating_sub(current_index)) / OBJECTS_PER_FILE;
    // Compute the milliseconds per file.
    let millis_per_file = timer.elapsed().as_millis() / num_files_done as u128;
    // Compute the heuristic slowdown factor (in millis).
    let slowdown = 100 * num_files_remaining as u128;
    // Compute the time remaining (in millis).
    let time_remaining = num_files_remaining as u128 * millis_per_file + slowdown;
    // Prepare the estimate message (in secs).
    let estimate = format!("(est. {} minutes remaining)", time_remaining / (60 * 1000));
    // Log the progress.
    info!("Synced up to {object_name} {current_index} of {cdn_end} - {percentage}% complete {}", estimate.dimmed());
}

#[cfg(test)]
mod tests {
    use crate::{
        blocks::{cdn_get, cdn_height, log_progress, BLOCKS_PER_FILE},
        load_blocks,
    };
    use snarkvm::prelude::{block::Block, Testnet3};

    use parking_lot::RwLock;
    use std::{sync::Arc, time::Instant};

    type CurrentNetwork = Testnet3;

    const TEST_BASE_URL: &str = "https://s3.us-west-1.amazonaws.com/testnet3.blocks/phase3";

    fn check_load_blocks(start: u32, end: Option<u32>, expected: usize) {
        let blocks = Arc::new(RwLock::new(Vec::new()));
        let blocks_clone = blocks.clone();
        let process = move |block: Block<CurrentNetwork>| {
            blocks_clone.write().push(block);
            Ok(())
        };

        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let completed_height = load_blocks(TEST_BASE_URL, start, end, Default::default(), process).await.unwrap();
            assert_eq!(blocks.read().len(), expected);
            if expected > 0 {
                assert_eq!(blocks.read().last().unwrap().height(), completed_height);
            }
            // Check they are sequential.
            for (i, block) in blocks.read().iter().enumerate() {
                assert_eq!(block.height(), start + i as u32);
            }
        });
    }

    #[test]
    fn test_load_blocks_1_to_50() {
        let start_height = 1;
        let end_height = Some(50);
        check_load_blocks(start_height, end_height, 49);
    }

    #[test]
    fn test_load_blocks_50_to_100() {
        let start_height = 50;
        let end_height = Some(100);
        check_load_blocks(start_height, end_height, 50);
    }

    #[test]
    fn test_load_blocks_1_to_123() {
        let start_height = 1;
        let end_height = Some(123);
        check_load_blocks(start_height, end_height, 122);
    }

    #[test]
    fn test_load_blocks_46_to_234() {
        let start_height = 46;
        let end_height = Some(234);
        check_load_blocks(start_height, end_height, 188);
    }

    #[test]
    fn test_cdn_height() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let height = cdn_height::<BLOCKS_PER_FILE>(TEST_BASE_URL).await.unwrap();
            assert!(height > 0);
        });
    }

    #[test]
    fn test_cdn_get() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let client = reqwest::Client::new();
            let height =
                cdn_get::<u32>(client, &format!("{TEST_BASE_URL}/testnet3/latest/height"), "height").await.unwrap();
            assert!(height > 0);
        });
    }

    #[test]
    fn test_log_progress() {
        // This test sanity checks that basic arithmetic is correct (i.e. no divide by zero, etc.).
        let timer = Instant::now();
        let cdn_range = &(0..100);
        let object_name = "blocks";
        log_progress::<10>(timer, 0, cdn_range, object_name);
        log_progress::<10>(timer, 10, cdn_range, object_name);
        log_progress::<10>(timer, 20, cdn_range, object_name);
        log_progress::<10>(timer, 30, cdn_range, object_name);
        log_progress::<10>(timer, 40, cdn_range, object_name);
        log_progress::<10>(timer, 50, cdn_range, object_name);
        log_progress::<10>(timer, 60, cdn_range, object_name);
        log_progress::<10>(timer, 70, cdn_range, object_name);
        log_progress::<10>(timer, 80, cdn_range, object_name);
        log_progress::<10>(timer, 90, cdn_range, object_name);
        log_progress::<10>(timer, 100, cdn_range, object_name);
    }
}
