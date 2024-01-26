// Copyright (C) 2013-2020 Blockstack PBC, a public benefit corporation
// Copyright (C) 2020-2024 Stacks Open Internet Foundation
//
// This program is free software: you can redistribute it and/or modify
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
// along with this program.  If not, see <http://www.gnu.org/licenses/>.
use std::collections::VecDeque;
use std::sync::mpsc::Sender;
use std::time::Duration;

use blockstack_lib::burnchains::Txid;
use blockstack_lib::chainstate::nakamoto::NakamotoBlock;
use blockstack_lib::chainstate::stacks::ThresholdSignature;
use blockstack_lib::net::api::postblock_proposal::BlockValidateResponse;
use hashbrown::{HashMap, HashSet};
use libsigner::{
    BlockRejection, BlockResponse, RejectCode, SignerEvent, SignerMessage, SignerRunLoop,
};
use slog::{slog_debug, slog_error, slog_info, slog_warn};
use stacks_common::codec::{read_next, StacksMessageCodec};
use stacks_common::util::hash::{Sha256Sum, Sha512Trunc256Sum};
use stacks_common::{debug, error, info, warn};
use wsts::common::{MerkleRoot, Signature};
use wsts::curve::ecdsa;
use wsts::curve::keys::PublicKey;
use wsts::net::{Message, NonceRequest, Packet, SignatureShareRequest};
use wsts::state_machine::coordinator::fire::Coordinator as FireCoordinator;
use wsts::state_machine::coordinator::{Config as CoordinatorConfig, Coordinator};
use wsts::state_machine::signer::Signer;
use wsts::state_machine::{OperationResult, PublicKeys, SignError};
use wsts::v2;

use crate::client::{retry_with_exponential_backoff, ClientError, StackerDB, StacksClient};
use crate::config::{Config, Network};

/// Which operation to perform
#[derive(PartialEq, Clone)]
pub enum RunLoopCommand {
    /// Generate a DKG aggregate public key
    Dkg,
    /// Sign a message
    Sign {
        /// The block to sign over
        block: NakamotoBlock,
        /// Whether to make a taproot signature
        is_taproot: bool,
        /// Taproot merkle root
        merkle_root: Option<MerkleRoot>,
    },
}

/// The RunLoop state
#[derive(PartialEq, Debug)]
pub enum State {
    // TODO: Uninitialized should indicate we need to replay events/configure the signer
    /// The runloop signer is uninitialized
    Uninitialized,
    /// The runloop is idle
    Idle,
    /// The runloop is executing a DKG round
    Dkg,
    /// The runloop is executing a signing round
    Sign,
}

/// Additional Info about a proposed block
pub struct BlockInfo {
    /// The block we are considering
    block: NakamotoBlock,
    /// Our vote on the block if we have one yet
    vote: Option<Vec<u8>>,
    /// Whether the block contents are valid
    valid: Option<bool>,
    /// The associated packet nonce request if we have one
    nonce_request: Option<NonceRequest>,
    /// Whether this block is already being signed over
    signing_round: bool,
}

impl BlockInfo {
    /// Create a new BlockInfo
    pub fn new(block: NakamotoBlock) -> Self {
        Self {
            block,
            vote: None,
            valid: None,
            nonce_request: None,
            signing_round: false,
        }
    }

    /// Create a new BlockInfo with an associated nonce request packet
    pub fn new_with_request(block: NakamotoBlock, nonce_request: NonceRequest) -> Self {
        Self {
            block,
            vote: None,
            valid: None,
            nonce_request: Some(nonce_request),
            signing_round: true,
        }
    }
}

/// The runloop for the stacks signer
pub struct RunLoop<C> {
    /// The timeout for events
    pub event_timeout: Duration,
    /// The coordinator for inbound messages
    pub coordinator: C,
    /// The signing round used to sign messages
    pub signing_round: Signer<v2::Signer>,
    /// The stacks node client
    pub stacks_client: StacksClient,
    /// The stacker db client
    pub stackerdb: StackerDB,
    /// Received Commands that need to be processed
    pub commands: VecDeque<RunLoopCommand>,
    /// The current state
    pub state: State,
    /// Wether mainnet or not
    pub mainnet: bool,
    /// Observed blocks that we have seen so far
    // TODO: cleanup storage and garbage collect this stuff
    pub blocks: HashMap<Sha512Trunc256Sum, BlockInfo>,
    /// Transactions that we expect to see in the next block
    // TODO: fill this in and do proper garbage collection
    pub transactions: Vec<Txid>,
}

impl<C: Coordinator> RunLoop<C> {
    /// Initialize the signer, reading the stacker-db state and setting the aggregate public key
    fn initialize(&mut self) -> Result<(), ClientError> {
        // Check if the aggregate key is set in the pox contract
        if let Some(key) = self.stacks_client.get_aggregate_public_key()? {
            debug!("Aggregate public key is set: {:?}", key);
            self.coordinator.set_aggregate_public_key(Some(key));
        } else {
            debug!("Aggregate public key is not set. Coordinator must trigger DKG...");
            // Update the state to IDLE so we don't needlessy requeue the DKG command.
            let (coordinator_id, _) =
                calculate_coordinator(&self.signing_round.public_keys, &self.stacks_client);
            if coordinator_id == self.signing_round.signer_id
                && self.commands.front() != Some(&RunLoopCommand::Dkg)
            {
                self.commands.push_front(RunLoopCommand::Dkg);
            }
        }
        self.state = State::Idle;
        Ok(())
    }

    /// Execute the given command and update state accordingly
    /// Returns true when it is successfully executed, else false
    fn execute_command(&mut self, command: &RunLoopCommand) -> bool {
        match command {
            RunLoopCommand::Dkg => {
                info!("Starting DKG");
                match self.coordinator.start_dkg_round() {
                    Ok(msg) => {
                        let ack = self
                            .stackerdb
                            .send_message_with_retry(self.signing_round.signer_id, msg.into());
                        debug!("ACK: {:?}", ack);
                        self.state = State::Dkg;
                        true
                    }
                    Err(e) => {
                        error!("Failed to start DKG: {:?}", e);
                        warn!("Resetting coordinator's internal state.");
                        self.coordinator.reset();
                        false
                    }
                }
            }
            RunLoopCommand::Sign {
                block,
                is_taproot,
                merkle_root,
            } => {
                let Ok(hash) = block.header.signer_signature_hash() else {
                    error!("Failed to sign block. Invalid signature hash.");
                    return false;
                };
                let block_info = self
                    .blocks
                    .entry(hash)
                    .or_insert_with(|| BlockInfo::new(block.clone()));
                if block_info.signing_round {
                    debug!("Received a sign command for a block we are already signing over. Ignore it.");
                    return false;
                }
                info!("Signing block: {:?}", block);
                match self.coordinator.start_signing_round(
                    &block.serialize_to_vec(),
                    *is_taproot,
                    *merkle_root,
                ) {
                    Ok(msg) => {
                        let ack = self
                            .stackerdb
                            .send_message_with_retry(self.signing_round.signer_id, msg.into());
                        debug!("ACK: {:?}", ack);
                        self.state = State::Sign;
                        block_info.signing_round = true;
                        true
                    }
                    Err(e) => {
                        error!("Failed to start signing message: {:?}", e);
                        warn!("Resetting coordinator's internal state.");
                        self.coordinator.reset();
                        false
                    }
                }
            }
        }
    }

    /// Attempt to process the next command in the queue, and update state accordingly
    fn process_next_command(&mut self) {
        match self.state {
            State::Uninitialized => {
                debug!(
                    "Signer is uninitialized. Waiting for aggregate public key from stacks node..."
                );
            }
            State::Idle => {
                if let Some(command) = self.commands.pop_front() {
                    while !self.execute_command(&command) {
                        warn!("Failed to execute command. Retrying...");
                    }
                } else {
                    debug!("Nothing to process. Waiting for command...");
                }
            }
            State::Dkg | State::Sign => {
                // We cannot execute the next command until the current one is finished...
                // Do nothing...
                debug!("Waiting for {:?} operation to finish", self.state);
            }
        }
    }

    /// Handle the block validate response returned from our prior calls to submit a block for validation
    fn handle_block_validate_response(
        &mut self,
        block_validate_response: BlockValidateResponse,
        res: Sender<Vec<OperationResult>>,
    ) {
        let transactions = &self.transactions;
        let (block_info, hash) = match block_validate_response {
            BlockValidateResponse::Ok(block_validate_ok) => {
                let Ok(hash) = block_validate_ok.block.header.signer_signature_hash() else {
                    self.broadcast_signature_hash_rejection(block_validate_ok.block);
                    return;
                };
                let block_info = self
                    .blocks
                    .entry(hash)
                    .or_insert(BlockInfo::new(block_validate_ok.block.clone()));
                block_info.valid = Some(true);
                (block_info, hash)
            }
            BlockValidateResponse::Reject(block_validate_reject) => {
                // There is no point in triggering a sign round for this block if validation failed from the stacks node
                let Ok(hash) = block_validate_reject.block.header.signer_signature_hash() else {
                    self.broadcast_signature_hash_rejection(block_validate_reject.block);
                    return;
                };
                let block_info = self
                    .blocks
                    .entry(hash)
                    .or_insert(BlockInfo::new(block_validate_reject.block.clone()));
                block_info.valid = Some(false);
                // Submit a rejection response to the .signers contract for miners
                // to observe so they know to send another block and to prove signers are doing work);
                if let Err(e) = self.stackerdb.send_message_with_retry(
                    self.signing_round.signer_id,
                    block_validate_reject.into(),
                ) {
                    warn!("Failed to send block rejection to stacker-db: {:?}", e);
                }
                (block_info, hash)
            }
        };

        if let Some(mut request) = block_info.nonce_request.take() {
            debug!("Received a block validate response from the stacks node for a block we already received a nonce request for. Responding to the nonce request...");
            // We have an associated nonce request. Respond to it
            Self::determine_vote(block_info, &mut request, transactions, hash);
            // Send the nonce request through with our vote
            let packet = Packet {
                msg: Message::NonceRequest(request),
                sig: vec![],
            };
            self.handle_packets(res, &[packet]);
        } else {
            let (coordinator_id, _) =
                calculate_coordinator(&self.signing_round.public_keys, &self.stacks_client);
            if block_info.valid.unwrap_or(false)
                && !block_info.signing_round
                && coordinator_id == self.signing_round.signer_id
            {
                debug!("Received a valid block proposal from the miner. Triggering a signing round over it...");
                // We are the coordinator. Trigger a signing round for this block
                self.commands.push_back(RunLoopCommand::Sign {
                    block: block_info.block.clone(),
                    is_taproot: false,
                    merkle_root: None,
                });
            } else {
                debug!("Ignoring block proposal.");
            }
        }
    }

    /// Handle signer messages submitted to signers stackerdb
    fn handle_signer_messages(
        &mut self,
        res: Sender<Vec<OperationResult>>,
        messages: Vec<SignerMessage>,
    ) {
        let (_coordinator_id, coordinator_public_key) =
            calculate_coordinator(&self.signing_round.public_keys, &self.stacks_client);
        let packets: Vec<Packet> = messages
            .into_iter()
            .filter_map(|msg| match msg {
                SignerMessage::BlockResponse(_) => None,
                SignerMessage::Packet(packet) => {
                    self.verify_packet(packet, &coordinator_public_key)
                }
            })
            .collect();
        self.handle_packets(res, &packets);
    }

    /// Handle proposed blocks submitted by the miners to stackerdb
    fn handle_proposed_blocks(&mut self, blocks: Vec<NakamotoBlock>) {
        for block in blocks {
            let Ok(hash) = block.header.signer_signature_hash() else {
                self.broadcast_signature_hash_rejection(block);
                continue;
            };
            // Store the block in our cache
            self.blocks.insert(hash, BlockInfo::new(block.clone()));
            // Submit the block for validation
            self.stacks_client
                .submit_block_for_validation(block)
                .unwrap_or_else(|e| {
                    warn!("Failed to submit block for validation: {:?}", e);
                });
        }
    }

    /// Process inbound packets as both a signer and a coordinator
    /// Will send outbound packets and operation results as appropriate
    fn handle_packets(&mut self, res: Sender<Vec<OperationResult>>, packets: &[Packet]) {
        let signer_outbound_messages = self
            .signing_round
            .process_inbound_messages(packets)
            .unwrap_or_else(|e| {
                error!("Failed to process inbound messages as a signer: {e}");
                vec![]
            });

        // Next process the message as the coordinator
        let (coordinator_outbound_messages, operation_results) = self
            .coordinator
            .process_inbound_messages(packets)
            .unwrap_or_else(|e| {
                error!("Failed to process inbound messages as a coordinator: {e}");
                (vec![], vec![])
            });

        if !operation_results.is_empty() {
            // We have finished a signing or DKG round, either successfully or due to error.
            // Regardless of the why, update our state to Idle as we should not expect the operation to continue.
            self.state = State::Idle;
            self.process_operation_results(&operation_results);
            self.send_operation_results(res, operation_results);
        }
        self.send_outbound_messages(signer_outbound_messages);
        self.send_outbound_messages(coordinator_outbound_messages);
    }

    /// Validate a signature share request, updating its message where appropriate.
    /// If the request is for a block it has already agreed to sign, it will overwrite the message with the agreed upon value
    /// Returns whether the request is valid or not.
    fn validate_signature_share_request(&self, request: &mut SignatureShareRequest) -> bool {
        let message_len = request.message.len();
        // Note that the message must always be either 32 bytes (the block hash) or 33 bytes (block hash + b'n')
        let hash_bytes = if message_len == 33 && request.message[32] == b'n' {
            // Pop off the 'n' byte from the block hash
            &request.message[..32]
        } else if message_len == 32 {
            // This is the block hash
            &request.message
        } else {
            // We will only sign across block hashes or block hashes + b'n' byte
            debug!("Received a signature share request for an unknown message stream. Reject it.");
            return false;
        };

        let Some(hash) = Sha512Trunc256Sum::from_bytes(hash_bytes) else {
            // We will only sign across valid block hashes
            debug!("Received a signature share request for an invalid block hash. Reject it.");
            return false;
        };
        match self.blocks.get(&hash).map(|block_info| &block_info.vote) {
            Some(Some(vote)) => {
                // Overwrite with our agreed upon value in case another message won majority or the coordinator is trying to cheat...
                request.message = vote.clone();
                true
            }
            Some(None) => {
                // We never agreed to sign this block. Reject it.
                // This can happen if the coordinator received enough votes to sign yes
                // or no on a block before we received validation from the stacks node.
                debug!("Received a signature share request for a block we never agreed to sign. Ignore it.");
                false
            }
            None => {
                // We will only sign across block hashes or block hashes + b'n' byte for
                // blocks we have seen a Nonce Request for (and subsequent validation)
                // We are missing the context here necessary to make a decision. Reject the block
                debug!("Received a signature share request from an unknown block. Reject it.");
                false
            }
        }
    }

    /// Validate a nonce request, updating its message appropriately.
    /// If the request is for a block, we will update the request message
    /// as either a hash indicating a vote no or the signature hash indicating a vote yes
    /// Returns whether the request is valid or not
    fn validate_nonce_request(&mut self, request: &mut NonceRequest) -> bool {
        let Some(block) = read_next::<NakamotoBlock, _>(&mut &request.message[..]).ok() else {
            // We currently reject anything that is not a block
            debug!("Received a nonce request for an unknown message stream. Reject it.");
            return false;
        };
        let Ok(hash) = block.header.signer_signature_hash() else {
            debug!(
                "Received a nonce request for a block with an invalid signature hash. Reject it"
            );
            return false;
        };
        let transactions = &self.transactions;
        let Some(block_info) = self.blocks.get_mut(&hash) else {
            // We have not seen this block before. Cache it. Send a RPC to the stacks node to validate it.
            debug!("We have received a block sign request for a block we have not seen before. Cache the nonce request and submit the block for validation...");
            // Store the block in our cache
            self.blocks.insert(
                hash,
                BlockInfo::new_with_request(block.clone(), request.clone()),
            );
            self.stacks_client
                .submit_block_for_validation(block)
                .unwrap_or_else(|e| {
                    warn!("Failed to submit block for validation: {:?}", e);
                });
            return false;
        };
        if block_info.valid.is_none() {
            // We have not yet received validation from the stacks node. Cache the request and wait for validation
            debug!("We have yet to receive validation from the stacks node for a nonce request. Cache the nonce request and wait for block validation...");
            block_info.nonce_request = Some(request.clone());
            return false;
        }
        Self::determine_vote(block_info, request, transactions, hash);
        true
    }

    /// Determine the vote for a block and update the block info and nonce request accordingly
    fn determine_vote(
        block_info: &mut BlockInfo,
        nonce_request: &mut NonceRequest,
        transactions: &[Txid],
        hash: Sha512Trunc256Sum,
    ) {
        let mut vote_bytes = hash.0.to_vec();
        // Validate the block contents
        if !block_info.valid.unwrap_or(false)
            || !transactions
                .iter()
                .all(|txid| block_info.block.txs.iter().any(|tx| &tx.txid() == txid))
        {
            // We don't like this block. Update the request to be across its hash with a byte indicating a vote no.
            debug!("Updating the request with a block hash with a vote no.");
            vote_bytes.push(b'n');
        } else {
            debug!("The block passed validation. Update the request with the signature hash.");
        }

        // Cache our vote
        block_info.vote = Some(vote_bytes.clone());
        nonce_request.message = vote_bytes;
    }

    /// Verify a chunk is a valid wsts packet. Returns the packet if it is valid, else None.
    /// NOTE: The packet will be updated if the signer wishes to respond to NonceRequest
    /// and SignatureShareRequests with a different message than what the coordinator originally sent.
    /// This is done to prevent a malicious coordinator from sending a different message than what was
    /// agreed upon and to support the case where the signer wishes to reject a block by voting no
    fn verify_packet(
        &mut self,
        mut packet: Packet,
        coordinator_public_key: &PublicKey,
    ) -> Option<Packet> {
        // We only care about verified wsts packets. Ignore anything else.
        if packet.verify(&self.signing_round.public_keys, coordinator_public_key) {
            match &mut packet.msg {
                Message::SignatureShareRequest(request) => {
                    if !self.validate_signature_share_request(request) {
                        return None;
                    }
                }
                Message::NonceRequest(request) => {
                    if !self.validate_nonce_request(request) {
                        return None;
                    }
                }
                _ => {
                    // Nothing to do for other message types
                }
            }
            Some(packet)
        } else {
            debug!("Failed to verify wsts packet: {:?}", &packet);
            None
        }
    }

    /// Processes the operation results, broadcasting block acceptance or rejection messages
    /// and DKG vote results accordingly
    fn process_operation_results(&mut self, operation_results: &[OperationResult]) {
        for operation_result in operation_results {
            // Signers only every trigger non-taproot signing rounds over blocks. Ignore SignTaproot results
            match operation_result {
                OperationResult::Sign(signature) => {
                    self.process_signature(signature);
                }
                OperationResult::SignTaproot(_) => {
                    debug!("Received a signature result for a taproot signature. Nothing to broadcast as we currently sign blocks with a FROST signature.");
                }
                OperationResult::Dkg(_point) => {
                    // TODO: cast the aggregate public key for the latest round here
                }
                OperationResult::SignError(e) => {
                    self.process_sign_error(e);
                }
                OperationResult::DkgError(e) => {
                    warn!("Received a DKG error: {:?}", e);
                }
            }
        }
    }

    /// Process a signature from a signing round by deserializing the signature and
    /// broadcasting an appropriate Reject or Approval message to stackerdb
    fn process_signature(&mut self, signature: &Signature) {
        // Deserialize the signature result and broadcast an appropriate Reject or Approval message to stackerdb
        let Some(aggregate_public_key) = &self.coordinator.get_aggregate_public_key() else {
            debug!("No aggregate public key set. Cannot validate signature...");
            return;
        };
        let message = self.coordinator.get_message();
        // This jankiness is because a coordinator could have signed a rejection we need to find the underlying block hash
        let block_hash_bytes = if message.len() > 32 {
            &message[..32]
        } else {
            &message
        };
        let Some(block_hash) = Sha512Trunc256Sum::from_bytes(block_hash_bytes) else {
            debug!("Received a signature result for a signature over a non-block. Nothing to broadcast.");
            return;
        };
        let Some(block_info) = self.blocks.remove(&block_hash) else {
            debug!("Received a signature result for a block we have not seen before. Ignoring...");
            return;
        };
        // This signature is no longer valid. Do not broadcast it.
        if !signature.verify(aggregate_public_key, &message) {
            warn!("Received an invalid signature result across the block. Do not broadcast it.");
            // TODO: should we reinsert it and trigger a sign round across the block again?
            return;
        }
        // Update the block signature hash with what the signers produced.
        let mut block = block_info.block;
        block.header.signer_signature = ThresholdSignature(signature.clone());

        let block_submission = if message == block_hash.0.to_vec() {
            // we agreed to sign the block hash. Return an approval message
            BlockResponse::Accepted(block).into()
        } else {
            // We signed a rejection message. Return a rejection message
            BlockRejection::new(block, RejectCode::SignedRejection).into()
        };

        // Submit signature result to miners to observe
        if let Err(e) = self
            .stackerdb
            .send_message_with_retry(self.signing_round.signer_id, block_submission)
        {
            warn!("Failed to send block submission to stacker-db: {:?}", e);
        }
    }

    /// Process a sign error from a signing round, broadcasting a rejection message to stackerdb accordingly
    fn process_sign_error(&mut self, e: &SignError) {
        warn!("Received a signature error: {:?}", e);
        match e {
            SignError::NonceTimeout(_valid_signers, _malicious_signers) => {
                //TODO: report these malicious signers
                debug!("Received a nonce timeout.");
            }
            SignError::InsufficientSigners(malicious_signers) => {
                let message = self.coordinator.get_message();
                let block = read_next::<NakamotoBlock, _>(&mut &message[..]).ok().unwrap_or({
                    // This is not a block so maybe its across its hash
                    // This jankiness is because a coordinator could have signed a rejection we need to find the underlying block hash
                    let block_hash_bytes = if message.len() > 32 {
                        &message[..32]
                    } else {
                        &message
                    };
                    let Some(block_hash) = Sha512Trunc256Sum::from_bytes(block_hash_bytes) else {
                        debug!("Received a signature result for a signature over a non-block. Nothing to broadcast.");
                        return;
                    };
                    let Some(block_info) = self.blocks.remove(&block_hash) else {
                        debug!("Received a signature result for a block we have not seen before. Ignoring...");
                        return;
                    };
                    block_info.block
                });
                // We don't have enough signers to sign the block. Broadcast a rejection
                let block_rejection = BlockRejection::new(
                    block,
                    RejectCode::InsufficientSigners(malicious_signers.clone()),
                );
                // Submit signature result to miners to observe
                if let Err(e) = self
                    .stackerdb
                    .send_message_with_retry(self.signing_round.signer_id, block_rejection.into())
                {
                    warn!("Failed to send block submission to stacker-db: {:?}", e);
                }
            }
            SignError::Aggregator(e) => {
                warn!("Received an aggregator error: {:?}", e);
            }
        }
        // TODO: should reattempt to sign the block here or should we just broadcast a rejection or do nothing and wait for the signers to propose a new block?
    }

    /// Send any operation results across the provided channel
    fn send_operation_results(
        &mut self,
        res: Sender<Vec<OperationResult>>,
        operation_results: Vec<OperationResult>,
    ) {
        let nmb_results = operation_results.len();
        match res.send(operation_results) {
            Ok(_) => {
                debug!("Successfully sent {} operation result(s)", nmb_results)
            }
            Err(e) => {
                warn!("Failed to send operation results: {:?}", e);
            }
        }
    }

    /// Sending all provided packets through stackerdb with a retry
    fn send_outbound_messages(&mut self, outbound_messages: Vec<Packet>) {
        debug!(
            "Sending {} messages to other stacker-db instances.",
            outbound_messages.len()
        );
        for msg in outbound_messages {
            let ack = self
                .stackerdb
                .send_message_with_retry(self.signing_round.signer_id, msg.into());
            if let Ok(ack) = ack {
                debug!("ACK: {:?}", ack);
            } else {
                warn!("Failed to send message to stacker-db instance: {:?}", ack);
            }
        }
    }

    /// Broadcast a block rejection due to an invalid block signature hash
    fn broadcast_signature_hash_rejection(&mut self, block: NakamotoBlock) {
        debug!("Broadcasting a block rejection due to a block with an invalid signature hash...");
        let block_rejection = BlockRejection::new(block, RejectCode::InvalidSignatureHash);
        // Submit signature result to miners to observe
        if let Err(e) = self
            .stackerdb
            .send_message_with_retry(self.signing_round.signer_id, block_rejection.into())
        {
            warn!("Failed to send block submission to stacker-db: {:?}", e);
        }
    }
}

impl From<&Config> for RunLoop<FireCoordinator<v2::Aggregator>> {
    /// Creates new runloop from a config
    fn from(config: &Config) -> Self {
        // TODO: this should be a config option
        // See: https://github.com/stacks-network/stacks-blockchain/issues/3914
        let threshold = ((config.signer_ids_public_keys.key_ids.len() * 7) / 10)
            .try_into()
            .unwrap();
        let dkg_threshold = ((config.signer_ids_public_keys.key_ids.len() * 9) / 10)
            .try_into()
            .unwrap();
        let total_signers = config
            .signer_ids_public_keys
            .signers
            .len()
            .try_into()
            .unwrap();
        let total_keys = config
            .signer_ids_public_keys
            .key_ids
            .len()
            .try_into()
            .unwrap();
        let key_ids = config
            .signer_key_ids
            .get(&config.signer_id)
            .unwrap()
            .clone();
        // signer uses a Vec<u32> for its key_ids, but coordinator uses a HashSet for each signer since it needs to do lots of lookups
        let signer_key_ids = config
            .signer_key_ids
            .iter()
            .map(|(i, ids)| (*i, ids.iter().copied().collect::<HashSet<u32>>()))
            .collect::<HashMap<u32, HashSet<u32>>>();

        let coordinator_config = CoordinatorConfig {
            threshold,
            dkg_threshold,
            num_signers: total_signers,
            num_keys: total_keys,
            message_private_key: config.message_private_key,
            dkg_public_timeout: config.dkg_public_timeout,
            dkg_private_timeout: config.dkg_private_timeout,
            dkg_end_timeout: config.dkg_end_timeout,
            nonce_timeout: config.nonce_timeout,
            sign_timeout: config.sign_timeout,
            signer_key_ids,
        };
        let coordinator = FireCoordinator::new(coordinator_config);
        let signing_round = Signer::new(
            threshold,
            total_signers,
            total_keys,
            config.signer_id,
            key_ids,
            config.message_private_key,
            config.signer_ids_public_keys.clone(),
        );
        let stacks_client = StacksClient::from(config);
        let stackerdb = StackerDB::from(config);
        RunLoop {
            event_timeout: config.event_timeout,
            coordinator,
            signing_round,
            stacks_client,
            stackerdb,
            commands: VecDeque::new(),
            state: State::Uninitialized,
            mainnet: config.network == Network::Mainnet,
            blocks: HashMap::new(),
            transactions: Vec::new(),
        }
    }
}

impl<C: Coordinator> SignerRunLoop<Vec<OperationResult>, RunLoopCommand> for RunLoop<C> {
    fn set_event_timeout(&mut self, timeout: Duration) {
        self.event_timeout = timeout;
    }

    fn get_event_timeout(&self) -> Duration {
        self.event_timeout
    }

    fn run_one_pass(
        &mut self,
        event: Option<SignerEvent>,
        cmd: Option<RunLoopCommand>,
        res: Sender<Vec<OperationResult>>,
    ) -> Option<Vec<OperationResult>> {
        info!(
            "Running one pass for signer ID# {}. Current state: {:?}",
            self.signing_round.signer_id, self.state
        );
        if let Some(command) = cmd {
            self.commands.push_back(command);
        }
        // TODO: This should be called every time as DKG can change at any time...but until we have the node
        // set up to receive cast votes...just do on initialization.
        if self.state == State::Uninitialized {
            let request_fn = || self.initialize().map_err(backoff::Error::transient);
            retry_with_exponential_backoff(request_fn)
                .expect("Failed to connect to initialize due to timeout. Stacks node may be down.");
        }
        // Process any arrived events
        debug!("Processing event: {:?}", event);
        match event {
            Some(SignerEvent::BlockValidationResponse(block_validate_response)) => {
                debug!("Received a block proposal result from the stacks node...");
                self.handle_block_validate_response(block_validate_response, res)
            }
            Some(SignerEvent::SignerMessages(messages)) => {
                debug!("Received messages from the other signers...");
                self.handle_signer_messages(res, messages);
            }
            Some(SignerEvent::ProposedBlocks(blocks)) => {
                debug!("Received block proposals from the miners...");
                self.handle_proposed_blocks(blocks);
            }
            None => {
                // No event. Do nothing.
                debug!("No event received")
            }
        }

        // The process the next command
        // Must be called AFTER processing the event as the state may update to IDLE due to said event.
        self.process_next_command();
        None
    }
}

/// Helper function for determining the coordinator public key given the the public keys
pub fn calculate_coordinator(
    public_keys: &PublicKeys,
    stacks_client: &StacksClient,
) -> (u32, ecdsa::PublicKey) {
    let stacks_tip_consensus_hash = match stacks_client.get_stacks_tip_consensus_hash() {
        Ok(hash) => hash,
        Err(e) => {
            error!("Error in fetching consensus hash: {:?}", e);
            return (0, public_keys.signers.get(&0).cloned().unwrap());
        }
    };
    debug!(
        "Using stacks_tip_consensus_hash {:?} for selecting coordinator",
        &stacks_tip_consensus_hash
    );

    // Create combined hash of each signer's public key with stacks_tip_consensus_hash
    let mut selection_ids = public_keys
        .signers
        .iter()
        .map(|(&id, pk)| {
            let pk_bytes = pk.to_bytes();
            let mut buffer =
                Vec::with_capacity(pk_bytes.len() + stacks_tip_consensus_hash.as_bytes().len());
            buffer.extend_from_slice(&pk_bytes[..]);
            buffer.extend_from_slice(stacks_tip_consensus_hash.as_bytes());
            let digest = Sha256Sum::from_data(&buffer).as_bytes().to_vec();
            (digest, id)
        })
        .collect::<Vec<_>>();

    // Sort the selection IDs based on the hash
    selection_ids.sort_by_key(|(hash, _)| hash.clone());

    // Get the first ID from the sorted list and retrieve its public key,
    // or default to the first signer if none are found
    selection_ids
        .first()
        .and_then(|(_, id)| public_keys.signers.get(id).map(|pk| (*id, pk.clone())))
        .unwrap_or((0, public_keys.signers.get(&0).cloned().unwrap()))
}

#[cfg(test)]
mod tests {
    use std::net::TcpListener;
    use std::thread::{sleep, spawn};

    use rand::distributions::Standard;
    use rand::Rng;

    use super::*;
    use crate::client::stacks_client::tests::{write_response, TestConfig};

    fn generate_random_consensus_hash() -> String {
        let rng = rand::thread_rng();
        let bytes: Vec<u8> = rng.sample_iter(Standard).take(20).collect();
        bytes.iter().map(|b| format!("{:02x}", b)).collect()
    }
    fn mock_stacks_client_response(mock_server: TcpListener, random_consensus: bool) {
        let consensus_hash = match random_consensus {
            true => generate_random_consensus_hash(),
            false => "static_hash_value".to_string(),
        };

        let response = format!(
            "HTTP/1.1 200 OK\n\n{{\"stacks_tip_consensus_hash\": \"{}\"}}",
            consensus_hash
        );

        spawn(move || {
            write_response(mock_server, response.as_bytes());
        });
        sleep(Duration::from_millis(100));
    }

    #[test]
    fn calculate_coordinator_should_produce_unique_results() {
        let config = Config::load_from_file("./src/tests/conf/signer-0.toml").unwrap();
        let number_of_tests = 5;

        let mut results = Vec::new();

        for _ in 0..number_of_tests {
            let test_config = TestConfig::new();
            mock_stacks_client_response(test_config.mock_server, true);

            let (coordinator_id, coordinator_public_key) =
                calculate_coordinator(&config.signer_ids_public_keys, &test_config.client);

            results.push((coordinator_id, coordinator_public_key));
        }

        // Check that not all coordinator IDs are the same
        let all_ids_same = results.iter().all(|&(id, _)| id == results[0].0);
        assert!(!all_ids_same, "Not all coordinator IDs should be the same");

        // Check that not all coordinator public keys are the same
        let all_keys_same = results
            .iter()
            .all(|&(_, ref key)| key.key.data == results[0].1.key.data);
        assert!(
            !all_keys_same,
            "Not all coordinator public keys should be the same"
        );
    }
    fn generate_test_results(random_consensus: bool, count: usize) -> Vec<(u32, ecdsa::PublicKey)> {
        let mut results = Vec::new();
        let config = Config::load_from_file("./src/tests/conf/signer-0.toml").unwrap();

        for _ in 0..count {
            let test_config = TestConfig::new();
            mock_stacks_client_response(test_config.mock_server, random_consensus);
            let result = calculate_coordinator(&config.signer_ids_public_keys, &test_config.client);
            results.push(result);
        }
        results
    }

    #[test]
    fn calculate_coordinator_results_should_vary_or_match_based_on_hash() {
        let results_with_random_hash = generate_test_results(true, 5);
        let all_ids_same = results_with_random_hash
            .iter()
            .all(|&(id, _)| id == results_with_random_hash[0].0);
        let all_keys_same = results_with_random_hash
            .iter()
            .all(|&(_, ref key)| key.key.data == results_with_random_hash[0].1.key.data);
        assert!(!all_ids_same, "Not all coordinator IDs should be the same");
        assert!(
            !all_keys_same,
            "Not all coordinator public keys should be the same"
        );

        let results_with_static_hash = generate_test_results(false, 5);
        let all_ids_same = results_with_static_hash
            .iter()
            .all(|&(id, _)| id == results_with_static_hash[0].0);
        let all_keys_same = results_with_static_hash
            .iter()
            .all(|&(_, ref key)| key.key.data == results_with_static_hash[0].1.key.data);
        assert!(all_ids_same, "All coordinator IDs should be the same");
        assert!(
            all_keys_same,
            "All coordinator public keys should be the same"
        );
    }
}
