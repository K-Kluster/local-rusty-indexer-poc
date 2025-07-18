use crate::BlockOrMany;
use itertools::FoldWhile::{Continue, Done};
use itertools::Itertools;
use kaspa_rpc_core::api::rpc::RpcApi;
use kaspa_rpc_core::{GetBlocksResponse, RpcHash, RpcHeader};
use kaspa_wrpc_client::KaspaRpcClient;
use tracing::{debug, error, info, trace, warn};

#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct Cursor {
    pub blue_score: u64,
    pub hash: RpcHash,
}

impl From<&RpcHeader> for Cursor {
    fn from(value: &RpcHeader) -> Self {
        Self {
            blue_score: value.blue_score,
            hash: value.hash,
        }
    }
}

impl Cursor {
    pub fn new(blue_score: u64, hash: RpcHash) -> Self {
        Self { blue_score, hash }
    }
}

/// Result of checking if sync target has been reached
#[derive(Debug, PartialEq, Eq)]
enum SyncTargetStatus {
    /// Target not yet reached, continue syncing from this cursor
    NotReached(Cursor),
    /// Target block found directly in the response
    TargetFoundDirectly,
    /// Target found indirectly via anticone resolution and selected child
    TargetFoundViaAnticone,
}

/// Configuration for the historical data syncer
#[derive(Debug)]
pub struct SyncConfig {
    /// Starting point for sync
    pub start_cursor: Cursor,
    /// Target endpoint for sync
    pub target_cursor: Cursor,
}

/// Manages historical data synchronization from Kaspa node
pub struct HistoricalDataSyncer {
    /// Current sync position
    current_cursor: Cursor,
    /// Target sync position
    target_cursor: Cursor,
    /// Candidates for anticone resolution during sync
    anticone_candidates: Vec<Cursor>,

    /// RPC client for communicating with Kaspa node
    rpc_client: KaspaRpcClient,
    /// Channel to send processed blocks to handler
    block_handler: flume::Sender<BlockOrMany>,
    /// Shutdown signal receiver
    shutdown_rx: tokio::sync::oneshot::Receiver<()>,

    /// Statistics for monitoring
    total_blocks_processed: u64,
    batches_processed: u64,
}

impl HistoricalDataSyncer {
    /// Creates a new historical data syncer
    pub fn new(
        rpc_client: KaspaRpcClient,
        start_cursor: Cursor,
        target_cursor: Cursor,
        block_handler: flume::Sender<BlockOrMany>,
        shutdown_rx: tokio::sync::oneshot::Receiver<()>,
    ) -> Self {
        info!(
            "Initializing historical data syncer: start_blue_score={}, target_blue_score={}, start_hash={:?}, target_hash={:?}",
            start_cursor.blue_score,
            target_cursor.blue_score,
            start_cursor.hash,
            target_cursor.hash
        );

        Self {
            current_cursor: start_cursor,
            target_cursor,
            anticone_candidates: Vec::new(),
            rpc_client,
            block_handler,
            shutdown_rx,
            total_blocks_processed: 0,
            batches_processed: 0,
        }
    }

    /// Starts the synchronization process
    pub async fn sync(&mut self) -> anyhow::Result<()> {
        info!("Starting historical data synchronization");

        loop {
            let fetch_next_batch = async || {
                loop {
                    let Ok(blocks) = self
                        .rpc_client
                        .get_blocks(Some(self.current_cursor.hash), true, true)
                        .await
                        .inspect_err(|e| error!("RPC get_blocks failed: {}", e))
                    else {
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                        continue;
                    };
                    return blocks;
                }
            };

            // Check for shutdown signal and fetch next batch
            let blocks_response = tokio::select! {
                biased;
                
                shutdown_result = &mut self.shutdown_rx => {
                    shutdown_result
                    .inspect(|_| info!("Shutdown signal received, stopping sync"))
                    .inspect_err(|e|  warn!("Shutdown receiver error: {}", e))?;

                    return Ok(())
                }
                response = fetch_next_batch() => response,
            };

            let batch_size = blocks_response.blocks.len();
            debug!("Processing batch of {} blocks", batch_size);

            // Process the batch and check if target is reached
            let target_status = self.process_blocks_batch(&blocks_response)?;

            // Send blocks to handler
            if let Err(e) = self
                .block_handler
                .send_async(BlockOrMany::Many(blocks_response.blocks))
                .await
            {
                error!("Failed to send blocks to handler: {}", e);
                return Err(anyhow::anyhow!("Block handler channel closed: {}", e));
            }

            self.batches_processed += 1;
            self.total_blocks_processed += batch_size as u64;

            // Log progress periodically
            if self.batches_processed % 100 == 0 {
                info!(
                    "Sync progress: {} batches processed, {} total blocks, current blue score: {}, target blue score: {}",
                    self.batches_processed,
                    self.total_blocks_processed,
                    self.current_cursor.blue_score,
                    self.target_cursor.blue_score,
                );
            }

            // Check if we've reached our target
            if self.is_sync_complete(&target_status) {
                info!(
                    "Synchronization completed successfully. Status: {:?}, Total blocks: {}, Total batches: {}",
                    target_status, self.total_blocks_processed, self.batches_processed
                );
                return Ok(());
            }
        }
    }

    /// Processes a batch of blocks and determines sync status
    fn process_blocks_batch(
        &mut self,
        response: &GetBlocksResponse,
    ) -> anyhow::Result<SyncTargetStatus> {
        let block_count = response.blocks.len();
        trace!("Processing {} blocks in current batch", block_count);

        if response.blocks.is_empty() {
            warn!("Received empty block batch");
            return Ok(SyncTargetStatus::NotReached(self.current_cursor));
        }

        let mut last_cursor = self.current_cursor;

        let target_status = response
            .block_hashes
            .iter()
            .zip(response.blocks.iter())
            .fold_while(
                SyncTargetStatus::NotReached(self.current_cursor),
                |_acc, (block_hash, block)| {
                    // Update cursor for each block processed
                    last_cursor = Cursor::new(block.header.blue_score, block.header.hash);

                    // Check if this block is our direct target
                    if block_hash == &self.target_cursor.hash {
                        debug!("Target block found directly: {:?}", block_hash);
                        return Done(SyncTargetStatus::TargetFoundDirectly);
                    }

                    // Process chain blocks for anticone resolution
                    if let Some(verbose_data) = &block.verbose_data {
                        if verbose_data.is_chain_block
                            && self.check_target_in_merge_sets(verbose_data)
                        {
                            debug!(
                                "Target found via anticone in block: {:?}, blue_score: {}",
                                block_hash, block.header.blue_score
                            );
                            return Done(SyncTargetStatus::TargetFoundViaAnticone);
                        }
                        // Add to anticone candidates if blue score qualifies
                        if block.header.blue_score >= self.target_cursor.blue_score {
                            let candidate = Cursor::new(block.header.blue_score, block.header.hash);
                            trace!("Adding anticone candidate: {:?}", candidate);
                            self.anticone_candidates.push(candidate);
                        }
                    } else {
                        warn!("Block missing verbose data: {:?}", block);
                    }

                    Continue(SyncTargetStatus::NotReached(last_cursor))
                },
            )
            .into_inner();

        // Update current cursor based on the result
        match &target_status {
            SyncTargetStatus::NotReached(cursor) => {
                self.current_cursor = *cursor;
                trace!("Updated current cursor to: {:?}", self.current_cursor);
            }
            SyncTargetStatus::TargetFoundDirectly | SyncTargetStatus::TargetFoundViaAnticone => {
                // Target found, cursor update not critical but keep it consistent
                self.current_cursor = last_cursor;
                trace!("Target found, final cursor: {:?}", self.current_cursor);
            }
        }

        Ok(target_status)
    }

    /// Checks if target or anticone candidates are found in merge sets
    fn check_target_in_merge_sets(
        &self,
        verbose_data: &kaspa_rpc_core::RpcBlockVerboseData,
    ) -> bool {
        // Check if target is directly in merge sets
        if verbose_data
            .merge_set_blues_hashes
            .contains(&self.target_cursor.hash)
            || verbose_data
                .merge_set_reds_hashes
                .contains(&self.target_cursor.hash)
        {
            return true;
        }

        // Check if any anticone candidates are in merge sets
        self.anticone_candidates.iter().any(|candidate| {
            verbose_data
                .merge_set_blues_hashes
                .contains(&candidate.hash)
                || verbose_data.merge_set_reds_hashes.contains(&candidate.hash)
        })
    }

    /// Determines if synchronization is complete based on target status
    fn is_sync_complete(&self, status: &SyncTargetStatus) -> bool {
        matches!(
            status,
            SyncTargetStatus::TargetFoundDirectly | SyncTargetStatus::TargetFoundViaAnticone
        )
    }

    /// Returns current sync statistics
    pub fn get_sync_stats(&self) -> SyncStats {
        SyncStats {
            total_blocks_processed: self.total_blocks_processed,
            batches_processed: self.batches_processed,
            current_blue_score: self.current_cursor.blue_score,
            target_blue_score: self.target_cursor.blue_score,
            anticone_candidates_count: self.anticone_candidates.len(),
        }
    }
}

/// Statistics for monitoring sync progress
#[derive(Debug, Clone)]
pub struct SyncStats {
    pub total_blocks_processed: u64,
    pub batches_processed: u64,
    pub current_blue_score: u64,
    pub target_blue_score: u64,
    pub anticone_candidates_count: usize,
}
