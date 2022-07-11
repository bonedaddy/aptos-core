// Copyright (c) Aptos
// SPDX-License-Identifier: Apache-2.0

use crate::{
    driver::DriverConfiguration,
    error::Error,
    logging::{LogEntry, LogSchema},
    metadata_storage::MetadataStorageInterface,
    storage_synchronizer::StorageSynchronizerInterface,
    utils,
    utils::{SpeculativeStreamState, PENDING_DATA_LOG_FREQ_SECS},
};
use aptos_config::config::BootstrappingMode;
use aptos_data_client::GlobalDataSummary;
use aptos_logger::{
    prelude::*,
    sample::{SampleRate, Sampling},
};
use aptos_types::{
    epoch_change::Verifier,
    epoch_state::EpochState,
    ledger_info::LedgerInfoWithSignatures,
    state_store::state_value::StateValueChunkWithProof,
    transaction::{TransactionListWithProof, TransactionOutputListWithProof, Version},
    waypoint::Waypoint,
};
use data_streaming_service::{
    data_notification::{DataNotification, DataPayload, NotificationId},
    data_stream::DataStreamListener,
    streaming_client::{DataStreamingClient, NotificationFeedback},
};
use futures::channel::oneshot;
use std::{collections::BTreeMap, sync::Arc, time::Duration};
use storage_interface::DbReader;

/// The expected version of the genesis transaction
pub const GENESIS_TRANSACTION_VERSION: u64 = 0;

/// A simple container for verified epoch states and epoch ending ledger infos
/// that have been fetched from the network.
pub(crate) struct VerifiedEpochStates {
    // If new epoch ending ledger infos have been fetched from the network
    fetched_epoch_ending_ledger_infos: bool,

    // The highest epoch ending version fetched thus far
    highest_fetched_epoch_ending_version: Version,

    // The latest epoch state that has been verified by the node
    latest_epoch_state: EpochState,

    // A map from versions to epoch ending ledger infos fetched from the network
    new_epoch_ending_ledger_infos: BTreeMap<Version, LedgerInfoWithSignatures>,

    // If the node has successfully verified the waypoint
    verified_waypoint: bool,
}

impl VerifiedEpochStates {
    pub fn new(latest_epoch_state: EpochState) -> Self {
        Self {
            fetched_epoch_ending_ledger_infos: false,
            highest_fetched_epoch_ending_version: 0,
            latest_epoch_state,
            new_epoch_ending_ledger_infos: BTreeMap::new(),
            verified_waypoint: false,
        }
    }

    /// Returns true iff the node has already fetched any new epoch
    /// ending ledger infos from the network.
    pub fn fetched_epoch_ending_ledger_infos(&self) -> bool {
        self.fetched_epoch_ending_ledger_infos
    }

    /// Sets `fetched_epoch_ending_ledger_infos` to true
    pub fn set_fetched_epoch_ending_ledger_infos(&mut self) {
        self.fetched_epoch_ending_ledger_infos = true;
    }

    /// Returns true iff the node has verified the waypoint
    pub fn verified_waypoint(&self) -> bool {
        self.verified_waypoint
    }

    /// Sets `verified_waypoint` to true
    pub fn set_verified_waypoint(&mut self) {
        self.verified_waypoint = true;
    }

    /// Verifies the given epoch ending ledger info, updates our latest
    /// trusted epoch state and attempts to verify any given waypoint.
    pub fn verify_epoch_ending_ledger_info(
        &mut self,
        epoch_ending_ledger_info: &LedgerInfoWithSignatures,
        waypoint: &Waypoint,
    ) -> Result<(), Error> {
        // Verify the ledger info against the latest epoch state
        self.latest_epoch_state
            .verify(epoch_ending_ledger_info)
            .map_err(|error| {
                Error::VerificationError(format!("Ledger info failed verification: {:?}", error))
            })?;

        // Update the latest epoch state with the next epoch
        if let Some(next_epoch_state) = epoch_ending_ledger_info.ledger_info().next_epoch_state() {
            self.highest_fetched_epoch_ending_version =
                epoch_ending_ledger_info.ledger_info().version();
            self.latest_epoch_state = next_epoch_state.clone();
            self.insert_new_epoch_ending_ledger_info(epoch_ending_ledger_info.clone());

            trace!(LogSchema::new(LogEntry::Bootstrapper).message(&format!(
                "Updated the latest epoch state to epoch: {:?}",
                self.latest_epoch_state.epoch
            )));
        } else {
            return Err(Error::VerificationError(
                "The ledger info was not epoch ending!".into(),
            ));
        }

        // Check if the ledger info corresponds to the trusted waypoint
        self.verify_waypoint(epoch_ending_ledger_info, waypoint)
    }

    /// Attempts to verify the waypoint using the new epoch ending ledger info
    fn verify_waypoint(
        &mut self,
        epoch_ending_ledger_info: &LedgerInfoWithSignatures,
        waypoint: &Waypoint,
    ) -> Result<(), Error> {
        if !self.verified_waypoint {
            // Fetch the waypoint and ledger info versions
            let waypoint_version = waypoint.version();
            let ledger_info = epoch_ending_ledger_info.ledger_info();
            let ledger_info_version = ledger_info.version();

            // Verify we haven't missed the waypoint
            if ledger_info_version > waypoint_version {
                return Err(Error::VerificationError(
                    format!("Failed to verify the waypoint: ledger info version is too high! Waypoint version: {:?}, ledger info version: {:?}",
                            waypoint_version, ledger_info_version)
                ));
            }

            // Check if we've found the ledger info corresponding to the waypoint version
            if ledger_info_version == waypoint_version {
                match waypoint.verify(ledger_info) {
                    Ok(()) => self.verified_waypoint = true,
                    Err(error) => {
                        return Err(Error::VerificationError(
                            format!("Failed to verify the waypoint: {:?}! Waypoint: {:?}, given ledger info: {:?}",
                                    error, waypoint, ledger_info)
                        ));
                    }
                }
            }
        }

        Ok(())
    }

    /// Adds an epoch ending ledger info to the new epoch ending ledger infos map
    fn insert_new_epoch_ending_ledger_info(
        &mut self,
        epoch_ending_ledger_info: LedgerInfoWithSignatures,
    ) {
        let ledger_info = epoch_ending_ledger_info.ledger_info();
        info!(LogSchema::new(LogEntry::Bootstrapper).message(&format!(
            "Adding a new epoch to the epoch ending ledger infos. Epoch: {:?}, Version: {:?}, Ends epoch: {:?}",
            ledger_info.epoch(), ledger_info.version(), ledger_info.ends_epoch(),
        )));

        // Insert the version to ledger info mapping
        let version = ledger_info.version();
        if let Some(epoch_ending_ledger_info) = self
            .new_epoch_ending_ledger_infos
            .insert(version, epoch_ending_ledger_info)
        {
            panic!(
                "Duplicate epoch ending ledger info found!\
                 Version: {:?}, \
                 ledger info: {:?}",
                version, epoch_ending_ledger_info,
            );
        }
    }

    /// Return all epoch ending ledger infos
    pub fn all_epoch_ending_ledger_infos(&self) -> Vec<LedgerInfoWithSignatures> {
        self.new_epoch_ending_ledger_infos
            .values()
            .cloned()
            .collect()
    }

    /// Returns any epoch ending ledger info associated with the given version
    pub fn get_epoch_ending_ledger_info(
        &self,
        version: Version,
    ) -> Option<LedgerInfoWithSignatures> {
        self.new_epoch_ending_ledger_infos.get(&version).cloned()
    }

    /// Returns the highest known ledger info we've fetched (if any)
    pub fn get_highest_known_ledger_info(&self) -> Option<LedgerInfoWithSignatures> {
        if !self.new_epoch_ending_ledger_infos.is_empty() {
            let highest_fetched_ledger_info = self
                .get_epoch_ending_ledger_info(self.highest_fetched_epoch_ending_version)
                .unwrap_or_else(|| {
                    panic!(
                        "The highest known ledger info for version: {:?} was not found!",
                        self.highest_fetched_epoch_ending_version
                    )
                });
            Some(highest_fetched_ledger_info)
        } else {
            None
        }
    }

    /// Returns the next epoch ending version after the given version (if one
    /// exists).
    pub fn next_epoch_ending_version(&self, version: Version) -> Option<Version> {
        // BTreeMap keys are iterated through in increasing key orders (i.e., versions)
        for (epoch_ending_version, _) in self.new_epoch_ending_ledger_infos.iter() {
            if *epoch_ending_version > version {
                return Some(*epoch_ending_version);
            }
        }
        None
    }
}

/// A simple container to manage data related to state value snapshot syncing
struct StateValueSyncer {
    // Whether or not a state snapshot receiver has been initialized
    initialized_state_snapshot_receiver: bool,

    // The epoch ending ledger info for the version we're syncing
    ledger_info_to_sync: Option<LedgerInfoWithSignatures>,

    // The next state value index to process (all state values before this have been
    // processed -- i.e., sent to the storage synchronizer).
    next_state_index_to_process: u64,

    // The transaction output (inc. info and proof) for the version we're syncing
    transaction_output_to_sync: Option<TransactionOutputListWithProof>,
}

impl StateValueSyncer {
    pub fn new() -> Self {
        Self {
            initialized_state_snapshot_receiver: false,
            ledger_info_to_sync: None,
            next_state_index_to_process: 0,
            transaction_output_to_sync: None,
        }
    }

    /// Updates the next state index to process
    pub fn update_next_state_index_to_process(&mut self, next_state_index_to_process: u64) {
        self.next_state_index_to_process = next_state_index_to_process;
    }
}

/// A simple component that manages the bootstrapping of the node
pub struct Bootstrapper<MetadataStorage, StorageSyncer, StreamingClient> {
    // The currently active data stream (provided by the data streaming service)
    active_data_stream: Option<DataStreamListener>,

    // The channel used to notify a listener of successful bootstrapping
    bootstrap_notifier_channel: Option<oneshot::Sender<Result<(), Error>>>,

    // If the node has completed bootstrapping
    bootstrapped: bool,

    // The config of the state sync driver
    driver_configuration: DriverConfiguration,

    // The storage to write metadata about the syncing progress
    metadata_storage: MetadataStorage,

    // The speculative state tracking the active data stream
    speculative_stream_state: Option<SpeculativeStreamState>,

    // The component used to sync state values (if downloading states)
    state_value_syncer: StateValueSyncer,

    // The client through which to stream data from the Aptos network
    streaming_client: StreamingClient,

    // The interface to read from storage
    storage: Arc<dyn DbReader>,

    // The storage synchronizer used to update local storage
    storage_synchronizer: StorageSyncer,

    // The epoch states verified by this node (held in memory)
    verified_epoch_states: VerifiedEpochStates,
}

impl<
        MetadataStorage: MetadataStorageInterface + Clone,
        StorageSyncer: StorageSynchronizerInterface + Clone,
        StreamingClient: DataStreamingClient + Clone,
    > Bootstrapper<MetadataStorage, StorageSyncer, StreamingClient>
{
    pub fn new(
        driver_configuration: DriverConfiguration,
        metadata_storage: MetadataStorage,
        streaming_client: StreamingClient,
        storage: Arc<dyn DbReader>,
        storage_synchronizer: StorageSyncer,
    ) -> Self {
        // Load the latest epoch state from storage
        let latest_epoch_state = utils::fetch_latest_epoch_state(storage.clone())
            .expect("Unable to fetch latest epoch state!");
        let verified_epoch_states = VerifiedEpochStates::new(latest_epoch_state);

        Self {
            state_value_syncer: StateValueSyncer::new(),
            active_data_stream: None,
            bootstrap_notifier_channel: None,
            bootstrapped: false,
            driver_configuration,
            metadata_storage,
            speculative_stream_state: None,
            streaming_client,
            storage,
            storage_synchronizer,
            verified_epoch_states,
        }
    }

    /// Returns true iff the node has already completed bootstrapping
    pub fn is_bootstrapped(&self) -> bool {
        self.bootstrapped
    }

    /// Marks bootstrapping as complete and notifies any listeners
    pub fn bootstrapping_complete(&mut self) -> Result<(), Error> {
        info!(LogSchema::new(LogEntry::Bootstrapper)
            .message("The node has successfully bootstrapped!"));
        self.bootstrapped = true;
        self.notify_listeners_if_bootstrapped()
    }

    /// Subscribes the specified channel to bootstrap completion notifications
    pub fn subscribe_to_bootstrap_notifications(
        &mut self,
        bootstrap_notifier_channel: oneshot::Sender<Result<(), Error>>,
    ) -> Result<(), Error> {
        if self.bootstrap_notifier_channel.is_some() {
            panic!("Only one boostrap subscriber is supported at a time!");
        }

        self.bootstrap_notifier_channel = Some(bootstrap_notifier_channel);
        self.notify_listeners_if_bootstrapped()
    }

    /// Notifies any listeners if we've now bootstrapped
    fn notify_listeners_if_bootstrapped(&mut self) -> Result<(), Error> {
        if self.bootstrapped {
            if let Some(notifier_channel) = self.bootstrap_notifier_channel.take() {
                if let Err(error) = notifier_channel.send(Ok(())) {
                    return Err(Error::CallbackSendFailed(format!(
                        "Bootstrap notification error: {:?}",
                        error
                    )));
                }
            }
        }

        Ok(())
    }

    /// Checks if the bootstrapper is able to make progress
    pub async fn drive_progress(
        &mut self,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<(), Error> {
        if self.is_bootstrapped() {
            return Err(Error::AlreadyBootstrapped(
                "The bootstrapper should not attempt to make progress!".into(),
            ));
        }

        if self.active_data_stream.is_some() {
            // We have an active data stream. Process any notifications!
            self.process_active_stream_notifications().await?;
        } else if self.storage_synchronizer.pending_storage_data() {
            // Wait for any pending data to be processed
            sample!(
                SampleRate::Duration(Duration::from_secs(PENDING_DATA_LOG_FREQ_SECS)),
                info!("Waiting for the storage synchronizer to handle pending data!")
            );
        } else {
            // Fetch a new data stream to start streaming data
            self.initialize_active_data_stream(global_data_summary)
                .await?;
        }

        // Check if we've now bootstrapped
        self.notify_listeners_if_bootstrapped()
    }

    /// Returns true iff the bootstrapper should continue to fetch epoch ending
    /// ledger infos (in order to make progress).
    fn should_fetch_epoch_ending_ledger_infos(&self) -> bool {
        !self
            .verified_epoch_states
            .fetched_epoch_ending_ledger_infos()
            || !self.verified_epoch_states.verified_waypoint()
    }

    /// Initializes an active data stream so that we can begin to process notifications
    async fn initialize_active_data_stream(
        &mut self,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<(), Error> {
        // Reset the chunk executor to flush any invalid state currently held in-memory
        self.storage_synchronizer.reset_chunk_executor()?;

        // Always fetch the new epoch ending ledger infos first
        if self.should_fetch_epoch_ending_ledger_infos() {
            return self
                .fetch_epoch_ending_ledger_infos(global_data_summary)
                .await;
        }

        // Get the highest synced and known ledger info versions
        let highest_synced_version = utils::fetch_latest_synced_version(self.storage.clone())?;
        let highest_known_ledger_info = self.get_highest_known_ledger_info()?;
        let highest_known_ledger_version = highest_known_ledger_info.ledger_info().version();

        // Bootstrap according to the mode
        match self.driver_configuration.config.bootstrapping_mode {
            BootstrappingMode::DownloadLatestStates => {
                if highest_synced_version == GENESIS_TRANSACTION_VERSION {
                    // We're syncing a new node. Check the progress and fetch the missing data.
                    if self.metadata_storage
                        .is_snapshot_sync_complete(highest_known_ledger_version)
                        .map_err(|error| {
                            Error::StorageError(format!(
                                "Failed to check the snapshot sync progress at version {:?}! Error {:?}",
                                highest_known_ledger_version, error
                            ))
                        })? {
                        self.bootstrapping_complete()
                    } else {
                        self.fetch_all_state_values(highest_known_ledger_info).await
                    }
                } else {
                    // This node has already synced some state. Ensure the node is not too far behind.
                    let num_versions_behind = highest_known_ledger_version
                        .checked_sub(highest_synced_version)
                        .ok_or_else(|| {
                            Error::IntegerOverflow(
                                "The number of versions behind has overflown!".into(),
                            )
                        })?;
                    if num_versions_behind
                        < self
                            .driver_configuration
                            .config
                            .num_versions_to_skip_snapshot_sync
                    {
                        self.bootstrapping_complete() // Skip the state snapshot sync
                    } else {
                        panic!("Snapshot syncing is currently unsupported for nodes with existing state! \
                        You are currently {:?} versions behind the latest snapshot version ({:?}). Either \
                        select a different syncing mode, or delete your storage and restart your node.",
                               num_versions_behind, highest_known_ledger_version);
                    }
                }
            }
            _ => {
                // We're either transaction or output syncing
                if highest_synced_version >= highest_known_ledger_version {
                    self.bootstrapping_complete()
                } else {
                    self.fetch_missing_transaction_data(
                        highest_synced_version,
                        highest_known_ledger_info,
                    )
                    .await
                }
            }
        }
    }

    /// Attempts to fetch a data notification from the active stream
    async fn fetch_next_data_notification(&mut self) -> Result<DataNotification, Error> {
        let max_stream_wait_time_ms = self.driver_configuration.config.max_stream_wait_time_ms;
        let result =
            utils::get_data_notification(max_stream_wait_time_ms, self.active_data_stream.as_mut())
                .await;
        if matches!(result, Err(Error::CriticalDataStreamTimeout(_))) {
            // If the stream has timed out too many times, we need to reset it
            warn!("Resetting the currently active data stream due to too many timeouts!");
            self.reset_active_stream();
        }
        result
    }

    /// Processes any notifications already pending on the active stream
    async fn process_active_stream_notifications(&mut self) -> Result<(), Error> {
        for _ in 0..self
            .driver_configuration
            .config
            .max_consecutive_stream_notifications
        {
            // Fetch and process any data notifications
            let data_notification = self.fetch_next_data_notification().await?;
            match data_notification.data_payload {
                DataPayload::StateValuesWithProof(state_value_chunk_with_proof) => {
                    self.process_state_values_payload(
                        data_notification.notification_id,
                        state_value_chunk_with_proof,
                    )
                    .await?;
                }
                DataPayload::EpochEndingLedgerInfos(epoch_ending_ledger_infos) => {
                    self.process_epoch_ending_payload(
                        data_notification.notification_id,
                        epoch_ending_ledger_infos,
                    )
                    .await?;
                }
                DataPayload::TransactionsWithProof(transactions_with_proof) => {
                    let payload_start_version = transactions_with_proof.first_transaction_version;
                    self.process_transaction_or_output_payload(
                        data_notification.notification_id,
                        Some(transactions_with_proof),
                        None,
                        payload_start_version,
                    )
                    .await?;
                }
                DataPayload::TransactionOutputsWithProof(transaction_outputs_with_proof) => {
                    let payload_start_version =
                        transaction_outputs_with_proof.first_transaction_output_version;
                    self.process_transaction_or_output_payload(
                        data_notification.notification_id,
                        None,
                        Some(transaction_outputs_with_proof),
                        payload_start_version,
                    )
                    .await?;
                }
                _ => {
                    return self
                        .handle_end_of_stream_or_invalid_payload(data_notification)
                        .await
                }
            }
        }

        Ok(())
    }

    /// Fetches state values (as required to bootstrap the node)
    async fn fetch_all_state_values(
        &mut self,
        highest_known_ledger_info: LedgerInfoWithSignatures,
    ) -> Result<(), Error> {
        // Verify we're trying to sync to an unchanging ledger info
        if let Some(ledger_info_to_sync) = &self.state_value_syncer.ledger_info_to_sync {
            if ledger_info_to_sync != &highest_known_ledger_info {
                panic!(
                    "Mismatch in ledger info to sync! Highest: {:?}, target: {:?}",
                    highest_known_ledger_info, ledger_info_to_sync
                );
            }
        } else {
            self.state_value_syncer.ledger_info_to_sync = Some(highest_known_ledger_info.clone());
        }

        // Fetch the data that we're missing
        let highest_known_ledger_version = highest_known_ledger_info.ledger_info().version();
        let data_stream = if self.state_value_syncer.transaction_output_to_sync.is_none() {
            // Fetch the transaction info first, before the states
            self.streaming_client
                .get_all_transaction_outputs(
                    highest_known_ledger_version,
                    highest_known_ledger_version,
                    highest_known_ledger_version,
                )
                .await?
        } else {
            // Get the last persisted state value index in storage
            let last_persisted_state_index = self
                .metadata_storage
                .get_last_persisted_state_value_index(highest_known_ledger_version)
                .map_err(|error| {
                    Error::StorageError(format!(
                        "Failed to get the last persisted state value index at version {:?}! Error: {:?}",
                        highest_known_ledger_version, error
                    ))
                })?;

            // Fetch the missing state values
            let next_state_index_to_process =
                last_persisted_state_index.checked_add(1).ok_or_else(|| {
                    Error::IntegerOverflow("The next state index to process has overflown!".into())
                })?;
            self.state_value_syncer
                .update_next_state_index_to_process(next_state_index_to_process);
            self.streaming_client
                .get_all_state_values(
                    highest_known_ledger_version,
                    Some(next_state_index_to_process),
                )
                .await?
        };
        self.active_data_stream = Some(data_stream);

        Ok(())
    }

    /// Fetches all missing transaction data in order to bootstrap the node
    async fn fetch_missing_transaction_data(
        &mut self,
        highest_synced_version: Version,
        highest_known_ledger_info: LedgerInfoWithSignatures,
    ) -> Result<(), Error> {
        let highest_known_ledger_version = highest_known_ledger_info.ledger_info().version();
        let next_version = highest_synced_version.checked_add(1).ok_or_else(|| {
            Error::IntegerOverflow("The next output version has overflown!".into())
        })?;
        let end_version = self
            .verified_epoch_states
            .next_epoch_ending_version(highest_synced_version)
            .expect("No higher epoch ending version known!");
        let data_stream = match self.driver_configuration.config.bootstrapping_mode {
            BootstrappingMode::ApplyTransactionOutputsFromGenesis => {
                self.streaming_client
                    .get_all_transaction_outputs(
                        next_version,
                        end_version,
                        highest_known_ledger_version,
                    )
                    .await?
            }
            BootstrappingMode::ExecuteTransactionsFromGenesis => {
                self.streaming_client
                    .get_all_transactions(
                        next_version,
                        end_version,
                        highest_known_ledger_version,
                        false,
                    )
                    .await?
            }
            bootstrapping_mode => {
                unreachable!("Bootstrapping mode not supported: {:?}", bootstrapping_mode)
            }
        };
        self.speculative_stream_state = Some(SpeculativeStreamState::new(
            utils::fetch_latest_epoch_state(self.storage.clone())?,
            Some(highest_known_ledger_info),
            highest_synced_version,
        ));
        self.active_data_stream = Some(data_stream);

        Ok(())
    }

    /// Fetches all epoch ending ledger infos (from the current epoch to the
    /// maximum that can be found by the data streaming service).
    async fn fetch_epoch_ending_ledger_infos(
        &mut self,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<(), Error> {
        // Verify the waypoint can be satisfied
        self.verify_waypoint_is_satisfiable(global_data_summary)?;

        // Get the highest advertised epoch that has ended
        let highest_advertised_epoch_end = global_data_summary
            .advertised_data
            .highest_epoch_ending_ledger_info()
            .ok_or_else(|| {
                Error::AdvertisedDataError(
                    "No highest advertised epoch end found in the network!".into(),
                )
            })?;

        // Fetch the highest epoch end known locally
        let highest_known_ledger_info = self.get_highest_known_ledger_info()?;
        let highest_known_ledger_info = highest_known_ledger_info.ledger_info();
        let highest_local_epoch_end = if highest_known_ledger_info.ends_epoch() {
            highest_known_ledger_info.epoch()
        } else if highest_known_ledger_info.epoch() > 0 {
            highest_known_ledger_info
                .epoch()
                .checked_sub(1)
                .ok_or_else(|| {
                    Error::IntegerOverflow("The highest local epoch end has overflown!".into())
                })?
        } else {
            unreachable!("Genesis should always end the first epoch!");
        };

        // Compare the highest local epoch end to the highest advertised epoch end
        if highest_local_epoch_end > highest_advertised_epoch_end {
            let error_message =
                format!(
                    "The highest local epoch end is higher than the advertised epoch end! Local: {:?}, advertised: {:?}",
                    highest_local_epoch_end, highest_advertised_epoch_end
                );
            return Err(Error::AdvertisedDataError(error_message));
        } else if highest_local_epoch_end < highest_advertised_epoch_end {
            info!(LogSchema::new(LogEntry::Bootstrapper).message(&format!(
                "Found higher epoch ending ledger infos in the network! Local: {:?}, advertised: {:?}",
                   highest_local_epoch_end, highest_advertised_epoch_end
            )));
            let next_epoch_end = highest_local_epoch_end.checked_add(1).ok_or_else(|| {
                Error::IntegerOverflow("The next epoch end has overflown!".into())
            })?;
            let epoch_ending_stream = self
                .streaming_client
                .get_all_epoch_ending_ledger_infos(next_epoch_end)
                .await?;
            self.active_data_stream = Some(epoch_ending_stream);
        } else if self.verified_epoch_states.verified_waypoint() {
            info!(LogSchema::new(LogEntry::Bootstrapper).message(
                "No new epoch ending ledger infos to fetch! All peers are in the same epoch!"
            ));
            self.verified_epoch_states
                .set_fetched_epoch_ending_ledger_infos();
        } else {
            return Err(Error::AdvertisedDataError("Our waypoint is unverified, but there's no higher epoch ending ledger infos advertised!".into()));
        };

        Ok(())
    }

    /// Verifies that connected peers have advertised data beyond our waypoint
    /// or that our waypoint is trivially satisfiable.
    fn verify_waypoint_is_satisfiable(
        &mut self,
        global_data_summary: &GlobalDataSummary,
    ) -> Result<(), Error> {
        // If our storage has already synced beyond our waypoint, nothing needs to be checked
        let latest_ledger_info = utils::fetch_latest_synced_ledger_info(self.storage.clone())?;
        let waypoint_version = self.driver_configuration.waypoint.version();
        if latest_ledger_info.ledger_info().version() >= waypoint_version {
            self.verified_epoch_states.set_verified_waypoint();
            return Ok(());
        }

        // Get the highest advertised synced ledger info version
        let highest_advertised_ledger_info = global_data_summary
            .advertised_data
            .highest_synced_ledger_info()
            .ok_or_else(|| {
                Error::AdvertisedDataError(
                    "No highest advertised ledger info found in the network!".into(),
                )
            })?;
        let highest_advertised_version = highest_advertised_ledger_info.ledger_info().version();

        // Compare the highest advertised version with our waypoint
        if highest_advertised_version < waypoint_version {
            Err(Error::AdvertisedDataError(
                format!(
                    "No advertised version higher than our waypoint! Highest version: {:?}, waypoint version: {:?}",
                    highest_advertised_version, waypoint_version
                )
            ))
        } else {
            Ok(())
        }
    }

    /// Verifies the start and end indices in the given state value chunk
    async fn verify_states_values_indices(
        &mut self,
        notification_id: NotificationId,
        state_value_chunk_with_proof: &StateValueChunkWithProof,
    ) -> Result<(), Error> {
        // Verify the payload start index is valid
        let expected_start_index = self.state_value_syncer.next_state_index_to_process;
        if expected_start_index != state_value_chunk_with_proof.first_index {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::VerificationError(format!(
                "The start index of the state values was invalid! Expected: {:?}, received: {:?}",
                expected_start_index, state_value_chunk_with_proof.first_index
            )));
        }

        // Verify the number of state values is valid
        let expected_num_state_values = state_value_chunk_with_proof
            .last_index
            .checked_sub(state_value_chunk_with_proof.first_index)
            .and_then(|version| version.checked_add(1)) // expected_num_state_values = last_index - first_index + 1
            .ok_or_else(|| {
                Error::IntegerOverflow("The expected number of state values has overflown!".into())
            })?;
        let num_state_values = state_value_chunk_with_proof.raw_values.len() as u64;
        if expected_num_state_values != num_state_values {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::VerificationError(format!(
                "The expected number of state values was invalid! Expected: {:?}, received: {:?}",
                expected_num_state_values, num_state_values,
            )));
        }

        // Verify the payload end index is valid
        let expected_end_index = state_value_chunk_with_proof
            .first_index
            .checked_add(num_state_values)
            .and_then(|version| version.checked_sub(1)) // expected_end_index = first_index + num_state_values - 1
            .ok_or_else(|| {
                Error::IntegerOverflow("The expected end of index has overflown!".into())
            })?;
        if expected_end_index != state_value_chunk_with_proof.last_index {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::VerificationError(format!(
                "The expected end index was invalid! Expected: {:?}, received: {:?}",
                expected_num_state_values, state_value_chunk_with_proof.last_index,
            )));
        }

        Ok(())
    }

    /// Process a single state value chunk with proof payload
    async fn process_state_values_payload(
        &mut self,
        notification_id: NotificationId,
        state_value_chunk_with_proof: StateValueChunkWithProof,
    ) -> Result<(), Error> {
        // Verify that we're expecting state value payloads
        let bootstrapping_mode = self.driver_configuration.config.bootstrapping_mode;
        if self.should_fetch_epoch_ending_ledger_infos()
            || !matches!(bootstrapping_mode, BootstrappingMode::DownloadLatestStates)
        {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::InvalidPayload(
                "Received an unexpected state values payload!".into(),
            ));
        }

        // Fetch the target ledger info and transaction info for bootstrapping
        let ledger_info_to_sync = self
            .state_value_syncer
            .ledger_info_to_sync
            .clone()
            .expect("Ledger info to sync is missing!");
        let transaction_output_to_sync = self
            .state_value_syncer
            .transaction_output_to_sync
            .clone()
            .expect("Transaction output to sync is missing!");

        // Initialize the state value synchronizer (if not already done)
        if !self.state_value_syncer.initialized_state_snapshot_receiver {
            // Fetch all verified epoch change proofs
            let epoch_change_proofs = self.verified_epoch_states.all_epoch_ending_ledger_infos();

            // Initialize the state value synchronizer
            let _ = self.storage_synchronizer.initialize_state_synchronizer(
                epoch_change_proofs,
                ledger_info_to_sync,
                transaction_output_to_sync.clone(),
            )?;
            self.state_value_syncer.initialized_state_snapshot_receiver = true;
        }

        // Verify the state values payload start and end indices
        self.verify_states_values_indices(notification_id, &state_value_chunk_with_proof)
            .await?;

        // Verify the chunk root hash matches the expected root hash
        let expected_root_hash = transaction_output_to_sync
            .proof
            .transaction_infos
            .first()
            .expect("Target transaction info should exist!")
            .ensure_state_checkpoint_hash()
            .expect("Must be at state checkpoint.");
        if state_value_chunk_with_proof.root_hash != expected_root_hash {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::VerificationError(format!(
                "The states chunk with proof root hash: {:?} didn't match the expected hash: {:?}!",
                state_value_chunk_with_proof.root_hash, expected_root_hash,
            )));
        }

        // Process the state values chunk and proof
        let last_state_value_index = state_value_chunk_with_proof.last_index;
        if let Err(error) = self
            .storage_synchronizer
            .save_state_values(notification_id, state_value_chunk_with_proof)
        {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::InvalidPayload(format!(
                "The states chunk with proof was invalid! Error: {:?}",
                error,
            )));
        }

        // Update the next state value index to process
        self.state_value_syncer.next_state_index_to_process =
            last_state_value_index.checked_add(1).ok_or_else(|| {
                Error::IntegerOverflow(
                    "The next state value index to process has overflown!".into(),
                )
            })?;

        Ok(())
    }

    /// Process a single epoch ending payload
    async fn process_epoch_ending_payload(
        &mut self,
        notification_id: NotificationId,
        epoch_ending_ledger_infos: Vec<LedgerInfoWithSignatures>,
    ) -> Result<(), Error> {
        // Verify that we're expecting epoch ending ledger info payloads
        if !self.should_fetch_epoch_ending_ledger_infos() {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::InvalidPayload(
                "Received an unexpected epoch ending payload!".into(),
            ));
        }

        // Verify the payload isn't empty
        if epoch_ending_ledger_infos.is_empty() {
            self.terminate_active_stream(notification_id, NotificationFeedback::EmptyPayloadData)
                .await?;
            return Err(Error::VerificationError(
                "The epoch ending payload was empty!".into(),
            ));
        }

        // Verify the epoch change proofs, update our latest epoch state and
        // verify our waypoint.
        for epoch_ending_ledger_info in epoch_ending_ledger_infos {
            if let Err(error) = self.verified_epoch_states.verify_epoch_ending_ledger_info(
                &epoch_ending_ledger_info,
                &self.driver_configuration.waypoint,
            ) {
                self.terminate_active_stream(
                    notification_id,
                    NotificationFeedback::PayloadProofFailed,
                )
                .await?;
                return Err(error);
            }
        }

        // TODO(joshlind): do we want to preemptively notify certain components
        // of the new reconfigurations?

        Ok(())
    }

    /// Process a single transaction or transaction output data payload
    async fn process_transaction_or_output_payload(
        &mut self,
        notification_id: NotificationId,
        transaction_list_with_proof: Option<TransactionListWithProof>,
        transaction_outputs_with_proof: Option<TransactionOutputListWithProof>,
        payload_start_version: Option<Version>,
    ) -> Result<(), Error> {
        // Verify that we're expecting transaction or output payloads
        let bootstrapping_mode = self.driver_configuration.config.bootstrapping_mode;
        if self.should_fetch_epoch_ending_ledger_infos()
            || (matches!(bootstrapping_mode, BootstrappingMode::DownloadLatestStates)
                && self.state_value_syncer.transaction_output_to_sync.is_some())
        {
            self.terminate_active_stream(notification_id, NotificationFeedback::InvalidPayloadData)
                .await?;
            return Err(Error::InvalidPayload(
                "Received an unexpected transaction or output payload!".into(),
            ));
        }

        // If we're state syncing, we expect a single transaction info
        if matches!(bootstrapping_mode, BootstrappingMode::DownloadLatestStates) {
            return self
                .verify_transaction_info_to_sync(
                    notification_id,
                    transaction_outputs_with_proof,
                    payload_start_version,
                )
                .await;
        }

        // Verify the payload starting version
        let expected_start_version = self
            .get_speculative_stream_state()
            .expected_next_version()?;
        let payload_start_version = self
            .verify_payload_start_version(
                notification_id,
                payload_start_version,
                expected_start_version,
            )
            .await?;

        // Get the expected proof ledger info for the payload
        let proof_ledger_info = self.get_speculative_stream_state().get_proof_ledger_info();

        // Get the end of epoch ledger info if the payload ends the epoch
        let end_of_epoch_ledger_info = self
            .get_end_of_epoch_ledger_info(
                notification_id,
                payload_start_version,
                transaction_list_with_proof.as_ref(),
                transaction_outputs_with_proof.as_ref(),
            )
            .await?;

        // Execute/apply and commit the transactions/outputs
        let num_transactions_or_outputs = match bootstrapping_mode {
            BootstrappingMode::ApplyTransactionOutputsFromGenesis => {
                if let Some(transaction_outputs_with_proof) = transaction_outputs_with_proof {
                    let num_transaction_outputs = transaction_outputs_with_proof
                        .transactions_and_outputs
                        .len();
                    self.storage_synchronizer.apply_transaction_outputs(
                        notification_id,
                        transaction_outputs_with_proof,
                        proof_ledger_info,
                        end_of_epoch_ledger_info,
                    )?;
                    num_transaction_outputs
                } else {
                    self.terminate_active_stream(
                        notification_id,
                        NotificationFeedback::PayloadTypeIsIncorrect,
                    )
                    .await?;
                    return Err(Error::InvalidPayload(
                        "Did not receive transaction outputs with proof!".into(),
                    ));
                }
            }
            BootstrappingMode::ExecuteTransactionsFromGenesis => {
                if let Some(transaction_list_with_proof) = transaction_list_with_proof {
                    let num_transactions = transaction_list_with_proof.transactions.len();
                    self.storage_synchronizer.execute_transactions(
                        notification_id,
                        transaction_list_with_proof,
                        proof_ledger_info,
                        end_of_epoch_ledger_info,
                    )?;
                    num_transactions
                } else {
                    self.terminate_active_stream(
                        notification_id,
                        NotificationFeedback::PayloadTypeIsIncorrect,
                    )
                    .await?;
                    return Err(Error::InvalidPayload(
                        "Did not receive transactions with proof!".into(),
                    ));
                }
            }
            bootstrapping_mode => {
                unreachable!("Bootstrapping mode not supported: {:?}", bootstrapping_mode)
            }
        };
        let synced_version = payload_start_version
            .checked_add(num_transactions_or_outputs as u64)
            .and_then(|version| version.checked_sub(1)) // synced_version = start + num txns/outputs - 1
            .ok_or_else(|| Error::IntegerOverflow("The synced version has overflown!".into()))?;
        self.get_speculative_stream_state()
            .update_synced_version(synced_version);

        Ok(())
    }

    /// Verifies the payload contains the transaction info we require to
    /// download all state values.
    async fn verify_transaction_info_to_sync(
        &mut self,
        notification_id: NotificationId,
        transaction_outputs_with_proof: Option<TransactionOutputListWithProof>,
        payload_start_version: Option<Version>,
    ) -> Result<(), Error> {
        // Verify the payload starting version
        let ledger_info_to_sync = self
            .state_value_syncer
            .ledger_info_to_sync
            .clone()
            .expect("Ledger info to sync is missing!");
        let expected_start_version = ledger_info_to_sync.ledger_info().version();
        let _ = self
            .verify_payload_start_version(
                notification_id,
                payload_start_version,
                expected_start_version,
            )
            .await?;

        // Verify the payload proof (the ledger info has already been verified)
        // and save the transaction output with proof.
        if let Some(transaction_outputs_with_proof) = transaction_outputs_with_proof {
            if transaction_outputs_with_proof.proof.transaction_infos.len() == 1 {
                match transaction_outputs_with_proof.verify(
                    ledger_info_to_sync.ledger_info(),
                    Some(expected_start_version),
                ) {
                    Ok(()) => {
                        self.state_value_syncer.transaction_output_to_sync =
                            Some(transaction_outputs_with_proof);
                    }
                    Err(error) => {
                        self.terminate_active_stream(
                            notification_id,
                            NotificationFeedback::PayloadProofFailed,
                        )
                        .await?;
                        return Err(Error::VerificationError(format!(
                            "Transaction outputs with proof is invalid! Error: {:?}",
                            error
                        )));
                    }
                }
            } else {
                self.terminate_active_stream(
                    notification_id,
                    NotificationFeedback::InvalidPayloadData,
                )
                .await?;
                return Err(Error::InvalidPayload(
                    "Payload does not contain a single transaction info!".into(),
                ));
            }
        } else {
            self.terminate_active_stream(
                notification_id,
                NotificationFeedback::PayloadTypeIsIncorrect,
            )
            .await?;
            return Err(Error::InvalidPayload(
                "Did not receive transaction outputs with proof!".into(),
            ));
        }

        Ok(())
    }

    /// Verifies the first payload version matches the version we wish to sync
    async fn verify_payload_start_version(
        &mut self,
        notification_id: NotificationId,
        payload_start_version: Option<Version>,
        expected_start_version: Version,
    ) -> Result<Version, Error> {
        if let Some(payload_start_version) = payload_start_version {
            if payload_start_version != expected_start_version {
                self.terminate_active_stream(
                    notification_id,
                    NotificationFeedback::InvalidPayloadData,
                )
                .await?;
                Err(Error::VerificationError(format!(
                    "The payload start version does not match the expected version! Start: {:?}, expected: {:?}",
                    payload_start_version, expected_start_version
                )))
            } else {
                Ok(payload_start_version)
            }
        } else {
            self.terminate_active_stream(notification_id, NotificationFeedback::EmptyPayloadData)
                .await?;
            Err(Error::VerificationError(
                "The playload starting version is missing!".into(),
            ))
        }
    }

    /// Returns the end of epoch ledger info for the given payload. If
    /// calculation fails, the active stream is terminated. Assumes the
    /// `payload_start_version` exists.
    async fn get_end_of_epoch_ledger_info(
        &mut self,
        notification_id: NotificationId,
        payload_start_version: Version,
        transaction_list_with_proof: Option<&TransactionListWithProof>,
        transaction_outputs_with_proof: Option<&TransactionOutputListWithProof>,
    ) -> Result<Option<LedgerInfoWithSignatures>, Error> {
        // Calculate the payload end version
        let num_versions = match self.driver_configuration.config.bootstrapping_mode {
            BootstrappingMode::ApplyTransactionOutputsFromGenesis => {
                if let Some(transaction_outputs_with_proof) = transaction_outputs_with_proof {
                    transaction_outputs_with_proof
                        .transactions_and_outputs
                        .len()
                } else {
                    self.terminate_active_stream(
                        notification_id,
                        NotificationFeedback::PayloadTypeIsIncorrect,
                    )
                    .await?;
                    return Err(Error::InvalidPayload(
                        "Did not receive transaction outputs with proof!".into(),
                    ));
                }
            }
            BootstrappingMode::ExecuteTransactionsFromGenesis => {
                if let Some(transaction_list_with_proof) = transaction_list_with_proof {
                    transaction_list_with_proof.transactions.len()
                } else {
                    self.terminate_active_stream(
                        notification_id,
                        NotificationFeedback::PayloadTypeIsIncorrect,
                    )
                    .await?;
                    return Err(Error::InvalidPayload(
                        "Did not receive transactions with proof!".into(),
                    ));
                }
            }
            bootstrapping_mode => {
                unimplemented!("Bootstrapping mode not supported: {:?}", bootstrapping_mode)
            }
        };
        let payload_end_version = payload_start_version
            .checked_add(num_versions as u64)
            .and_then(|v| v.checked_sub(1))
            .ok_or_else(|| {
                Error::IntegerOverflow("The payload end version has overflown!".into())
            })?; // payload_end_version = payload_start_version + num_versions - 1

        // Find any epoch ending ledger info at the payload end version
        Ok(self
            .verified_epoch_states
            .get_epoch_ending_ledger_info(payload_end_version))
    }

    /// Returns the highest known ledger info (including the newly fetch ones)
    fn get_highest_known_ledger_info(&self) -> Result<LedgerInfoWithSignatures, Error> {
        // Fetch the highest synced ledger info from storage
        let mut highest_known_ledger_info =
            utils::fetch_latest_synced_ledger_info(self.storage.clone())?;

        // Fetch the highest verified ledger info (from the network) and take
        // the maximum.
        if let Some(verified_ledger_info) =
            self.verified_epoch_states.get_highest_known_ledger_info()
        {
            if verified_ledger_info.ledger_info().version()
                > highest_known_ledger_info.ledger_info().version()
            {
                highest_known_ledger_info = verified_ledger_info;
            }
        }
        Ok(highest_known_ledger_info)
    }

    /// Handles the end of stream notification or an invalid payload by
    /// terminating the stream appropriately.
    async fn handle_end_of_stream_or_invalid_payload(
        &mut self,
        data_notification: DataNotification,
    ) -> Result<(), Error> {
        self.reset_active_stream();

        utils::handle_end_of_stream_or_invalid_payload(
            &mut self.streaming_client,
            data_notification,
        )
        .await
    }

    /// Terminates the currently active stream with the provided feedback
    pub async fn terminate_active_stream(
        &mut self,
        notification_id: NotificationId,
        notification_feedback: NotificationFeedback,
    ) -> Result<(), Error> {
        self.reset_active_stream();

        utils::terminate_stream_with_feedback(
            &mut self.streaming_client,
            notification_id,
            notification_feedback,
        )
        .await
    }

    /// Returns the speculative stream state. Assumes that the state exists.
    fn get_speculative_stream_state(&mut self) -> &mut SpeculativeStreamState {
        self.speculative_stream_state
            .as_mut()
            .expect("Speculative stream state does not exist!")
    }

    /// Resets the currently active data stream and speculative state
    fn reset_active_stream(&mut self) {
        self.speculative_stream_state = None;
        self.active_data_stream = None;
    }

    /// Returns the verified epoch states struct for testing purposes.
    #[cfg(test)]
    pub(crate) fn get_verified_epoch_states(&mut self) -> &mut VerifiedEpochStates {
        &mut self.verified_epoch_states
    }
}
