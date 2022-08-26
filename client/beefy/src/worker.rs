// This file is part of Substrate.

// Copyright (C) 2021-2022 Parity Technologies (UK) Ltd.
// SPDX-License-Identifier: GPL-3.0-or-later WITH Classpath-exception-2.0

// This program is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE. See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with this program. If not, see <https://www.gnu.org/licenses/>.

use std::{
	collections::{BTreeMap, BTreeSet, VecDeque},
	fmt::Debug,
	marker::PhantomData,
	sync::Arc,
};

use codec::{Codec, Decode, Encode};
use futures::StreamExt;
use log::{debug, error, info, log_enabled, trace, warn};
use parking_lot::Mutex;

use sc_client_api::{Backend, FinalityNotification};
use sc_network_common::{
	protocol::event::Event as NetEvent,
	service::{NetworkEventStream, NetworkRequest},
};
use sc_network_gossip::GossipEngine;

use sp_api::{BlockId, ProvideRuntimeApi};
use sp_arithmetic::traits::{AtLeast32Bit, Saturating};
use sp_consensus::SyncOracle;
use sp_mmr_primitives::MmrApi;
use sp_runtime::{
	generic::OpaqueDigestItemId,
	traits::{Block, Header, NumberFor},
	SaturatedConversion,
};

use beefy_primitives::{
	crypto::{AuthorityId, Signature},
	known_payload_ids, BeefyApi, Commitment, ConsensusLog, MmrRootHash, Payload, SignedCommitment,
	ValidatorSet, VersionedFinalityProof, VoteMessage, BEEFY_ENGINE_ID, GENESIS_AUTHORITY_SET_ID,
};

use crate::{
	communication::{
		gossip::{topic, GossipValidator},
		request_response::outgoing_request::OnDemandJustififactionsEngine,
	},
	error::Error,
	justification::BeefyVersionedFinalityProof,
	keystore::BeefyKeystore,
	metric_inc, metric_set,
	metrics::Metrics,
	round::Rounds,
	BeefyVoterLinks, Client, KnownPeers,
};

enum RoundAction {
	Drop,
	Process,
	Enqueue,
}

/// Responsible for the voting strategy.
/// It chooses which incoming votes to accept and which votes to generate.
struct VoterOracle<B: Block> {
	/// Queue of known sessions. Keeps track of voting rounds (block numbers) within each session.
	///
	/// There are three voter states coresponding to three queue states:
	/// 1. voter uninitialized: queue empty,
	/// 2. up-to-date - all mandatory blocks leading up to current GRANDPA finalized:
	///    queue has ONE element, the 'current session' where `mandatory_done == true`,
	/// 3. lagging behind GRANDPA: queue has [1, N] elements, where all `mandatory_done == false`.
	///    In this state, everytime a session gets its mandatory block BEEFY finalized, it's
	///    popped off the queue, eventually getting to state `2. up-to-date`.
	sessions: VecDeque<Rounds<Payload, B>>,
	/// Min delta in block numbers between two blocks, BEEFY should vote on.
	min_block_delta: u32,
}

impl<B: Block> VoterOracle<B> {
	pub fn new(min_block_delta: u32) -> Self {
		Self {
			sessions: VecDeque::new(),
			// Always target at least one block better than current best beefy.
			min_block_delta: min_block_delta.max(1),
		}
	}

	/// Return mutable reference to rounds pertaining to first session in the queue.
	/// Voting will always happen at the head of the queue.
	pub fn rounds_mut(&mut self) -> Option<&mut Rounds<Payload, B>> {
		self.sessions.front_mut()
	}

	/// Add new observed session to the Oracle.
	pub fn add_session(&mut self, rounds: Rounds<Payload, B>) {
		self.sessions.push_back(rounds);
		self.try_prune();
	}

	/// Prune the queue to keep the Oracle in one of the expected three states.
	///
	/// Call this function on each BEEFY finality,
	/// or at the very least on each BEEFY mandatory block finality.
	pub fn try_prune(&mut self) {
		if self.sessions.len() > 1 {
			// when there's multiple sessions, only keep the `!mandatory_done()` ones.
			self.sessions.retain(|s| !s.mandatory_done())
		}
	}

	/// Return `(A, B)` tuple representing inclusive [A, B] interval of votes to accept.
	pub fn accepted_interval(
		&self,
		best_grandpa: NumberFor<B>,
	) -> Result<(NumberFor<B>, NumberFor<B>), Error> {
		let rounds = self.sessions.front().ok_or(Error::UninitSession)?;

		if rounds.mandatory_done() {
			// There's only one session active and its mandatory is done.
			// Accept any GRANDPA finalized vote.
			Ok((rounds.session_start(), best_grandpa.into()))
		} else {
			// There's at least one session with mandatory not done.
			// Only accept votes for the mandatory block in the front of queue.
			Ok((rounds.session_start(), rounds.session_start()))
		}
	}

	/// Utility function to quickly decide what to do for each round.
	pub fn triage_round(
		&self,
		round: NumberFor<B>,
		best_grandpa: NumberFor<B>,
	) -> Result<RoundAction, Error> {
		let (start, end) = self.accepted_interval(best_grandpa)?;
		if start <= round && round <= end {
			Ok(RoundAction::Process)
		} else if round > end {
			Ok(RoundAction::Enqueue)
		} else {
			Ok(RoundAction::Drop)
		}
	}

	/// Return `Some(number)` if we should be voting on block `number`,
	/// return `None` if there is no block we should vote on.
	pub fn voting_target(
		&self,
		best_beefy: Option<NumberFor<B>>,
		best_grandpa: NumberFor<B>,
	) -> Option<NumberFor<B>> {
		let rounds = if let Some(r) = self.sessions.front() {
			r
		} else {
			debug!(target: "beefy", "🥩 No voting round started");
			return None
		};

		// `target` is guaranteed > `best_beefy` since `min_block_delta` is at least `1`.
		let target =
			vote_target(best_grandpa, best_beefy, rounds.session_start(), self.min_block_delta);
		trace!(
			target: "beefy",
			"🥩 best beefy: #{:?}, best finalized: #{:?}, current_vote_target: {:?}",
			best_beefy,
			best_grandpa,
			target
		);
		target
	}
}

pub(crate) struct WorkerParams<B: Block, BE, C, R, N> {
	pub client: Arc<C>,
	pub backend: Arc<BE>,
	pub runtime: Arc<R>,
	pub network: N,
	pub key_store: BeefyKeystore,
	pub known_peers: Arc<Mutex<KnownPeers<B>>>,
	pub gossip_engine: GossipEngine<B>,
	pub gossip_validator: Arc<GossipValidator<B>>,
	pub links: BeefyVoterLinks<B>,
	pub metrics: Option<Metrics>,
	pub min_block_delta: u32,
}

/// A BEEFY worker plays the BEEFY protocol
pub(crate) struct BeefyWorker<B: Block, BE, C, R, N> {
	// utilities
	client: Arc<C>,
	backend: Arc<BE>,
	runtime: Arc<R>,
	network: N,
	key_store: BeefyKeystore,

	// communication
	known_peers: Arc<Mutex<KnownPeers<B>>>,
	gossip_engine: GossipEngine<B>,
	gossip_validator: Arc<GossipValidator<B>>,
	on_demand_justifications: OnDemandJustififactionsEngine<B, N>,

	// channels
	/// Links between the block importer, the background voter and the RPC layer.
	links: BeefyVoterLinks<B>,

	// voter state
	/// BEEFY client metrics.
	metrics: Option<Metrics>,
	/// Best block we received a GRANDPA finality for.
	best_grandpa_block_header: <B as Block>::Header,
	/// Best block a BEEFY voting round has been concluded for.
	best_beefy_block: Option<NumberFor<B>>,
	/// Buffer holding votes for future processing.
	pending_votes: BTreeMap<NumberFor<B>, Vec<VoteMessage<NumberFor<B>, AuthorityId, Signature>>>,
	/// Buffer holding justifications for future processing.
	pending_justifications: BTreeMap<NumberFor<B>, Vec<BeefyVersionedFinalityProof<B>>>,
	/// Chooses which incoming votes to accept and which votes to generate.
	voting_oracle: VoterOracle<B>,
}

impl<B, BE, C, R, N> BeefyWorker<B, BE, C, R, N>
where
	B: Block + Codec,
	BE: Backend<B>,
	C: Client<B, BE>,
	R: ProvideRuntimeApi<B>,
	R::Api: BeefyApi<B> + MmrApi<B, MmrRootHash>,
	N: NetworkEventStream + NetworkRequest + SyncOracle + Send + Sync + Clone + 'static,
{
	/// Return a new BEEFY worker instance.
	///
	/// Note that a BEEFY worker is only fully functional if a corresponding
	/// BEEFY pallet has been deployed on-chain.
	///
	/// The BEEFY pallet is needed in order to keep track of the BEEFY authority set.
	pub(crate) fn new(worker_params: WorkerParams<B, BE, C, R, N>) -> Self {
		let WorkerParams {
			client,
			backend,
			runtime,
			key_store,
			network,
			gossip_engine,
			gossip_validator,
			known_peers,
			links,
			metrics,
			min_block_delta,
		} = worker_params;

		let last_finalized_header = client
			.expect_header(BlockId::number(client.info().finalized_number))
			.expect("latest block always has header available; qed.");

		let on_demand_justifications = OnDemandJustififactionsEngine::new(
			network.clone(),
			// FIXME: use right protocol name.
			"TODO: FIXME: proto-name-here".into(),
		);

		BeefyWorker {
			client: client.clone(),
			backend,
			runtime,
			network,
			known_peers,
			key_store,
			gossip_engine,
			gossip_validator,
			on_demand_justifications,
			links,
			metrics,
			best_grandpa_block_header: last_finalized_header,
			best_beefy_block: None,
			pending_votes: BTreeMap::new(),
			pending_justifications: BTreeMap::new(),
			voting_oracle: VoterOracle::new(min_block_delta),
		}
	}

	/// Simple wrapper that gets MMR root from header digests or from client state.
	fn get_mmr_root_digest(&self, header: &B::Header) -> Option<MmrRootHash> {
		find_mmr_root_digest::<B>(header).or_else(|| {
			self.runtime
				.runtime_api()
				.mmr_root(&BlockId::hash(header.hash()))
				.ok()
				.and_then(|r| r.ok())
		})
	}

	/// Verify `active` validator set for `block` against the key store
	///
	/// We want to make sure that we have _at least one_ key in our keystore that
	/// is part of the validator set, that's because if there are no local keys
	/// then we can't perform our job as a validator.
	///
	/// Note that for a non-authority node there will be no keystore, and we will
	/// return an error and don't check. The error can usually be ignored.
	fn verify_validator_set(
		&self,
		block: &NumberFor<B>,
		active: &ValidatorSet<AuthorityId>,
	) -> Result<(), Error> {
		let active: BTreeSet<&AuthorityId> = active.validators().iter().collect();

		let public_keys = self.key_store.public_keys()?;
		let store: BTreeSet<&AuthorityId> = public_keys.iter().collect();

		if store.intersection(&active).count() == 0 {
			let msg = "no authority public key found in store".to_string();
			debug!(target: "beefy", "🥩 for block {:?} {}", block, msg);
			Err(Error::Keystore(msg))
		} else {
			Ok(())
		}
	}

	/// Handle session changes by starting new voting round for mandatory blocks.
	fn init_session_at(
		&mut self,
		validator_set: ValidatorSet<AuthorityId>,
		new_session_start: NumberFor<B>,
	) {
		debug!(target: "beefy", "🥩 New active validator set: {:?}", validator_set);
		metric_set!(self, beefy_validator_set_id, validator_set.id());

		// BEEFY should produce the mandatory block of each session.
		if let Some(active_session) = self.voting_oracle.rounds_mut() {
			if !active_session.mandatory_done() {
				debug!(
					target: "beefy", "🥩 New session {} while active session {} is still lagging.",
					validator_set.id(),
					active_session.validator_set_id(),
				);
				metric_inc!(self, beefy_lagging_sessions);
			}
		}

		if log_enabled!(target: "beefy", log::Level::Debug) {
			// verify the new validator set - only do it if we're also logging the warning
			let _ = self.verify_validator_set(&new_session_start, &validator_set);
		}

		let id = validator_set.id();
		self.voting_oracle.add_session(Rounds::new(new_session_start, validator_set));
		info!(target: "beefy", "🥩 New Rounds for validator set id: {:?} with session_start {:?}", id, new_session_start);
	}

	fn handle_finality_notification(&mut self, notification: &FinalityNotification<B>) {
		debug!(target: "beefy", "🥩 Finality notification: {:?}", notification);
		let header = &notification.header;

		if *header.number() > *self.best_grandpa_block_header.number() {
			// update best GRANDPA finalized block we have seen
			self.best_grandpa_block_header = header.clone();

			// Check for and enqueue potential new session.
			if let Some(new_validator_set) = find_authorities_change::<B>(header) {
				self.init_session_at(new_validator_set, *header.number());
				// TODO: when adding SYNC protocol, fire up a request for justification for this
				// mandatory block here.
				self.on_demand_justifications.fire_request_for(*header.number());
			}
		}
	}

	/// Based on [VoterOracle] this vote is either processed here or enqueued for later.
	fn triage_incoming_vote(
		&mut self,
		vote: VoteMessage<NumberFor<B>, AuthorityId, Signature>,
	) -> Result<(), Error> {
		let block_num = vote.commitment.block_number;
		let best_grandpa = *self.best_grandpa_block_header.number();
		match self.voting_oracle.triage_round(block_num, best_grandpa)? {
			RoundAction::Process => self.handle_vote(
				(vote.commitment.payload, vote.commitment.block_number),
				(vote.id, vote.signature),
				false,
			)?,
			RoundAction::Enqueue => {
				debug!(target: "beefy", "🥩 Buffer vote for round: {:?}.", block_num);
				self.pending_votes.entry(block_num).or_default().push(vote)
			},
			RoundAction::Drop => (),
		};
		Ok(())
	}

	/// Based on [VoterOracle] this justification is either processed here or enqueued for later.
	///
	/// Expects `justification` to be valid.
	fn triage_incoming_justif(
		&mut self,
		justification: BeefyVersionedFinalityProof<B>,
	) -> Result<(), Error> {
		let signed_commitment = match justification {
			VersionedFinalityProof::V1(ref sc) => sc,
		};
		let block_num = signed_commitment.commitment.block_number;
		let best_grandpa = *self.best_grandpa_block_header.number();
		match self.voting_oracle.triage_round(block_num, best_grandpa)? {
			RoundAction::Process => self.finalize(justification),
			RoundAction::Enqueue => {
				debug!(target: "beefy", "🥩 Buffer justification for round: {:?}.", block_num);
				self.pending_justifications.entry(block_num).or_default().push(justification)
			},
			RoundAction::Drop => (),
		};
		Ok(())
	}

	fn handle_vote(
		&mut self,
		round: (Payload, NumberFor<B>),
		vote: (AuthorityId, Signature),
		self_vote: bool,
	) -> Result<(), Error> {
		self.gossip_validator.note_round(round.1);

		let rounds = self.voting_oracle.rounds_mut().ok_or(Error::UninitSession)?;

		if rounds.add_vote(&round, vote, self_vote) {
			if let Some(signatures) = rounds.try_conclude(&round) {
				self.gossip_validator.conclude_round(round.1);

				let block_num = round.1;
				let commitment = Commitment {
					payload: round.0,
					block_number: block_num,
					validator_set_id: rounds.validator_set_id(),
				};

				let finality_proof =
					VersionedFinalityProof::V1(SignedCommitment { commitment, signatures });

				metric_set!(self, beefy_round_concluded, block_num);

				info!(target: "beefy", "🥩 Round #{} concluded, finality_proof: {:?}.", round.1, finality_proof);

				if let Err(e) = self.backend.append_justification(
					BlockId::Number(block_num),
					(BEEFY_ENGINE_ID, finality_proof.clone().encode()),
				) {
					debug!(target: "beefy", "🥩 Error {:?} on appending justification: {:?}", e, finality_proof);
				}

				// We created the `finality_proof` and know to be valid.
				self.finalize(finality_proof);
			}
		}
		Ok(())
	}

	/// Provide BEEFY finality for block based on `finality_proof`:
	/// 1. Prune irrelevant past sessions from the oracle,
	/// 2. Set BEEFY best block,
	/// 3. Send best block hash and `finality_proof` to RPC worker.
	///
	/// Expects `finality proof` to be valid.
	fn finalize(&mut self, finality_proof: BeefyVersionedFinalityProof<B>) {
		// Prune any now "finalized" sessions from queue.
		self.voting_oracle.try_prune();
		let signed_commitment = match finality_proof {
			VersionedFinalityProof::V1(ref sc) => sc,
		};
		let block_num = signed_commitment.commitment.block_number;
		if Some(block_num) > self.best_beefy_block {
			// Set new best BEEFY block number.
			self.best_beefy_block = Some(block_num);
			metric_set!(self, beefy_best_block, block_num);

			self.client.hash(block_num).ok().flatten().map(|hash| {
				self.links
					.to_rpc_best_block_sender
					.notify(|| Ok::<_, ()>(hash))
					.expect("forwards closure result; the closure always returns Ok; qed.")
			});

			self.links
				.to_rpc_justif_sender
				.notify(|| Ok::<_, ()>(finality_proof))
				.expect("forwards closure result; the closure always returns Ok; qed.");
		} else {
			debug!(target: "beefy", "🥩 Can't set best beefy to older: {}", block_num);
		}
	}

	/// Handle previously buffered justifications and votes that now land in the voting interval.
	fn try_pending_justif_and_votes(&mut self) -> Result<(), Error> {
		let best_grandpa = *self.best_grandpa_block_header.number();
		let _ph = PhantomData::<B>::default();

		fn to_process_for<B: Block, T>(
			pending: &mut BTreeMap<NumberFor<B>, Vec<T>>,
			(start, end): (NumberFor<B>, NumberFor<B>),
			_: PhantomData<B>,
		) -> BTreeMap<NumberFor<B>, Vec<T>> {
			// These are still pending.
			let still_pending = pending.split_off(&end.saturating_add(1u32.into()));
			// These can be processed.
			let to_handle = pending.split_off(&start);
			// The rest can be dropped.
			*pending = still_pending;
			// Return ones to process.
			to_handle
		}

		// Process pending justifications.
		let interval = self.voting_oracle.accepted_interval(best_grandpa)?;
		if !self.pending_justifications.is_empty() {
			let justifs_to_handle = to_process_for(&mut self.pending_justifications, interval, _ph);
			for (num, justifications) in justifs_to_handle.into_iter() {
				debug!(target: "beefy", "🥩 Handle buffered justifications for: {:?}.", num);
				for justif in justifications.into_iter() {
					self.finalize(justif);
				}
			}
		}

		// Process pending votes.
		let interval = self.voting_oracle.accepted_interval(best_grandpa)?;
		if !self.pending_votes.is_empty() {
			let votes_to_handle = to_process_for(&mut self.pending_votes, interval, _ph);
			for (num, votes) in votes_to_handle.into_iter() {
				debug!(target: "beefy", "🥩 Handle buffered votes for: {:?}.", num);
				for v in votes.into_iter() {
					if let Err(err) = self.handle_vote(
						(v.commitment.payload, v.commitment.block_number),
						(v.id, v.signature),
						false,
					) {
						error!(target: "beefy", "🥩 Error handling buffered vote: {}", err);
					};
				}
			}
		}
		Ok(())
	}

	/// Decide if should vote, then vote.. or don't..
	fn try_to_vote(&mut self) -> Result<(), Error> {
		// Vote if there's now a new vote target.
		if let Some(target) = self
			.voting_oracle
			.voting_target(self.best_beefy_block, *self.best_grandpa_block_header.number())
		{
			metric_set!(self, beefy_should_vote_on, target);
			self.do_vote(target)?;
		}
		Ok(())
	}

	/// Create and gossip Signed Commitment for block number `target_number`.
	///
	/// Also handle this self vote by calling `self.handle_vote()` for it.
	fn do_vote(&mut self, target_number: NumberFor<B>) -> Result<(), Error> {
		debug!(target: "beefy", "🥩 Try voting on {}", target_number);

		// Most of the time we get here, `target` is actually `best_grandpa`,
		// avoid asking `client` for header in that case.
		let target_header = if target_number == *self.best_grandpa_block_header.number() {
			self.best_grandpa_block_header.clone()
		} else {
			self.client.expect_header(BlockId::Number(target_number)).map_err(|err| {
				let err_msg = format!(
					"Couldn't get header for block #{:?} (error: {:?}), skipping vote..",
					target_number, err
				);
				Error::Backend(err_msg)
			})?
		};
		let target_hash = target_header.hash();

		let mmr_root = if let Some(hash) = self.get_mmr_root_digest(&target_header) {
			hash
		} else {
			warn!(target: "beefy", "🥩 No MMR root digest found for: {:?}", target_hash);
			return Ok(())
		};
		let payload = Payload::new(known_payload_ids::MMR_ROOT_ID, mmr_root.encode());

		let rounds = self.voting_oracle.rounds_mut().ok_or(Error::UninitSession)?;
		if !rounds.should_self_vote(&(payload.clone(), target_number)) {
			debug!(target: "beefy", "🥩 Don't double vote for block number: {:?}", target_number);
			return Ok(())
		}
		let (validators, validator_set_id) = (rounds.validators(), rounds.validator_set_id());

		let authority_id = if let Some(id) = self.key_store.authority_id(validators) {
			debug!(target: "beefy", "🥩 Local authority id: {:?}", id);
			id
		} else {
			debug!(target: "beefy", "🥩 Missing validator id - can't vote for: {:?}", target_hash);
			return Ok(())
		};

		let commitment = Commitment { payload, block_number: target_number, validator_set_id };
		let encoded_commitment = commitment.encode();

		let signature = match self.key_store.sign(&authority_id, &encoded_commitment) {
			Ok(sig) => sig,
			Err(err) => {
				warn!(target: "beefy", "🥩 Error signing commitment: {:?}", err);
				return Ok(())
			},
		};

		trace!(
			target: "beefy",
			"🥩 Produced signature using {:?}, is_valid: {:?}",
			authority_id,
			BeefyKeystore::verify(&authority_id, &signature, &encoded_commitment)
		);

		let message = VoteMessage { commitment, id: authority_id, signature };

		let encoded_message = message.encode();

		metric_inc!(self, beefy_votes_sent);

		debug!(target: "beefy", "🥩 Sent vote message: {:?}", message);

		if let Err(err) = self.handle_vote(
			(message.commitment.payload, message.commitment.block_number),
			(message.id, message.signature),
			true,
		) {
			error!(target: "beefy", "🥩 Error handling self vote: {}", err);
		}

		self.gossip_engine.gossip_message(topic::<B>(), encoded_message, false);

		Ok(())
	}

	/// Wait for BEEFY runtime pallet to be available.
	async fn wait_for_runtime_pallet(&mut self) {
		let mut gossip_engine = &mut self.gossip_engine;
		let mut finality_stream = self.client.finality_notification_stream().fuse();
		loop {
			futures::select! {
				notif = finality_stream.next() => {
					let notif = match notif {
						Some(notif) => notif,
						None => break
					};
					let at = BlockId::hash(notif.header.hash());
				if let Some(active) = self.runtime.runtime_api().validator_set(&at).ok().flatten() {
					if active.id() == GENESIS_AUTHORITY_SET_ID {
						// When starting from genesis, there is no session boundary digest.
						// Just initialize `rounds` to Block #1 as BEEFY mandatory block.
						self.init_session_at(active, 1u32.into());
					}
					// In all other cases, we just go without `rounds` initialized, meaning the
					// worker won't vote until it witnesses a session change.
					// Once we'll implement 'initial sync' (catch-up), the worker will be able to
					// start voting right away.
					self.handle_finality_notification(&notif);
					if let Err(err) = self.try_to_vote() {
						debug!(target: "beefy", "🥩 {}", err);
					}
					break
				} else {
					trace!(target: "beefy", "🥩 Finality notification: {:?}", notif);
					debug!(target: "beefy", "🥩 Waiting for BEEFY pallet to become available...");
				}
				},
				_ = gossip_engine => {
					break
				}
			}
		}
	}

	/// Main loop for BEEFY worker.
	///
	/// Wait for BEEFY runtime pallet to be available, then start the main async loop
	/// which is driven by finality notifications and gossiped votes.
	pub(crate) async fn run(mut self) {
		info!(target: "beefy", "🥩 run BEEFY worker, best grandpa: #{:?}.", self.best_grandpa_block_header.number());
		self.wait_for_runtime_pallet().await;

		let mut network_events = self.network.event_stream("network-gossip").fuse();
		let mut finality_notifications = self.client.finality_notification_stream().fuse();
		let mut votes = Box::pin(
			self.gossip_engine
				.messages_for(topic::<B>())
				.filter_map(|notification| async move {
					trace!(target: "beefy", "🥩 Got vote message: {:?}", notification);

					VoteMessage::<NumberFor<B>, AuthorityId, Signature>::decode(
						&mut &notification.message[..],
					)
					.ok()
				})
				.fuse(),
		);
		let mut block_import_justif = self.links.from_block_import_justif_stream.subscribe().fuse();

		loop {
			let mut gossip_engine = &mut self.gossip_engine;
			// Wait for, and handle external events.
			// The branches below only change 'state', actual voting happen afterwards,
			// based on the new resulting 'state'.
			futures::select_biased! {
				notification = finality_notifications.next() => {
					if let Some(notification) = notification {
						self.handle_finality_notification(&notification);
					} else {
						error!(target: "beefy", "🥩 Finality stream terminated, closing worker.");
						return;
					}
				},
				justif = block_import_justif.next() => {
					if let Some(justif) = justif {
						// Block import justifications have already been verified to be valid
						// by `BeefyBlockImport`.
						if let Err(err) = self.triage_incoming_justif(justif) {
							debug!(target: "beefy", "🥩 {}", err);
						}
					} else {
						error!(target: "beefy", "🥩 Block import stream terminated, closing worker.");
						return;
					}
				},
				// TODO: join this stream's branch with the one above; how? .. ¯\_(ツ)_/¯
				justif = self.on_demand_justifications.next() => {
					if let Some(justif) = justif {
						// TODO: make sure proofs are verified before consuming.
						if let Err(err) = self.triage_incoming_justif(justif) {
							debug!(target: "beefy", "🥩 {}", err);
						}
					} else {
						error!(target: "beefy", "🥩 On demand justifications stream terminated, closing worker.");
						return;
					}
				},
				vote = votes.next() => {
					if let Some(vote) = vote {
						// Votes have already been verified to be valid by the gossip validator.
						if let Err(err) = self.triage_incoming_vote(vote) {
							debug!(target: "beefy", "🥩 {}", err);
						}
					} else {
						error!(target: "beefy", "🥩 Votes gossiping stream terminated, closing worker.");
						return;
					}
				},
				net_event = network_events.next() => {
					if let Some(net_event) = net_event {
						self.handle_network_event(net_event);
					} else {
						error!(target: "beefy", "🥩 Network events stream terminated, closing worker.");
						return;
					}
				},
				_ = gossip_engine => {
					error!(target: "beefy", "🥩 Gossip engine has terminated, closing worker.");
					return;
				}
			}

			// Don't bother acting on 'state' changes during major sync.
			if !self.network.is_major_syncing() {
				// Handle pending justifications and/or votes for now GRANDPA finalized blocks.
				if let Err(err) = self.try_pending_justif_and_votes() {
					debug!(target: "beefy", "🥩 {}", err);
				}

				// There were external events, 'state' is changed, author a vote if needed/possible.
				if let Err(err) = self.try_to_vote() {
					debug!(target: "beefy", "🥩 {}", err);
				}
			}
		}
	}

	/// Update known peers based on network events.
	fn handle_network_event(&mut self, event: NetEvent) {
		match event {
			NetEvent::SyncConnected { remote } => {
				self.known_peers.lock().add_new(remote);
			},
			NetEvent::SyncDisconnected { remote } => {
				self.known_peers.lock().remove(&remote);
			},
			// We don't care about other events.
			_ => (),
		}
	}
}

/// Extract the MMR root hash from a digest in the given header, if it exists.
fn find_mmr_root_digest<B>(header: &B::Header) -> Option<MmrRootHash>
where
	B: Block,
{
	let id = OpaqueDigestItemId::Consensus(&BEEFY_ENGINE_ID);

	let filter = |log: ConsensusLog<AuthorityId>| match log {
		ConsensusLog::MmrRoot(root) => Some(root),
		_ => None,
	};
	header.digest().convert_first(|l| l.try_to(id).and_then(filter))
}

/// Scan the `header` digest log for a BEEFY validator set change. Return either the new
/// validator set or `None` in case no validator set change has been signaled.
fn find_authorities_change<B>(header: &B::Header) -> Option<ValidatorSet<AuthorityId>>
where
	B: Block,
{
	let id = OpaqueDigestItemId::Consensus(&BEEFY_ENGINE_ID);

	let filter = |log: ConsensusLog<AuthorityId>| match log {
		ConsensusLog::AuthoritiesChange(validator_set) => Some(validator_set),
		_ => None,
	};
	header.digest().convert_first(|l| l.try_to(id).and_then(filter))
}

/// Calculate next block number to vote on.
///
/// Return `None` if there is no voteable target yet.
fn vote_target<N>(
	best_grandpa: N,
	best_beefy: Option<N>,
	session_start: N,
	min_delta: u32,
) -> Option<N>
where
	N: AtLeast32Bit + Copy + Debug,
{
	// if the mandatory block (session_start) does not have a beefy justification yet,
	// we vote on it
	let target = match best_beefy {
		None => {
			debug!(
				target: "beefy",
				"🥩 vote target - mandatory block: #{:?}",
				session_start,
			);
			session_start
		},
		Some(bbb) if bbb < session_start => {
			debug!(
				target: "beefy",
				"🥩 vote target - mandatory block: #{:?}",
				session_start,
			);
			session_start
		},
		Some(bbb) => {
			let diff = best_grandpa.saturating_sub(bbb) + 1u32.into();
			let diff = diff.saturated_into::<u32>() / 2;
			let target = bbb + min_delta.max(diff.next_power_of_two()).into();

			debug!(
				target: "beefy",
				"🥩 vote target - diff: {:?}, next_power_of_two: {:?}, target block: #{:?}",
				diff,
				diff.next_power_of_two(),
				target,
			);

			target
		},
	};

	// Don't vote for targets until they've been finalized
	// (`target` can be > `best_grandpa` when `min_delta` is big enough).
	if target > best_grandpa {
		None
	} else {
		Some(target)
	}
}

#[cfg(test)]
pub(crate) mod tests {
	use super::*;
	use crate::{
		communication::notification::{BeefyBestBlockStream, BeefyVersionedFinalityProofStream},
		keystore::tests::Keyring,
		tests::{
			create_beefy_keystore, get_beefy_streams, make_beefy_ids, two_validators::TestApi,
			BeefyPeer, BeefyTestNet, BEEFY_PROTOCOL_NAME,
		},
		BeefyRPCLinks,
	};

	use futures::{executor::block_on, future::poll_fn, task::Poll};

	use sc_client_api::HeaderBackend;
	use sc_network::NetworkService;
	use sc_network_test::{PeersFullClient, TestNetFactory};
	use sp_api::HeaderT;
	use substrate_test_runtime_client::{
		runtime::{Block, Digest, DigestItem, Header, H256},
		Backend,
	};

	fn create_beefy_worker(
		peer: &BeefyPeer,
		key: &Keyring,
		min_block_delta: u32,
	) -> BeefyWorker<Block, Backend, PeersFullClient, TestApi, Arc<NetworkService<Block, H256>>> {
		let keystore = create_beefy_keystore(*key);

		let (to_rpc_justif_sender, from_voter_justif_stream) =
			BeefyVersionedFinalityProofStream::<Block>::channel();
		let (to_rpc_best_block_sender, from_voter_best_beefy_stream) =
			BeefyBestBlockStream::<Block>::channel();
		let (_, from_block_import_justif_stream) =
			BeefyVersionedFinalityProofStream::<Block>::channel();

		let beefy_rpc_links =
			BeefyRPCLinks { from_voter_justif_stream, from_voter_best_beefy_stream };
		*peer.data.beefy_rpc_links.lock() = Some(beefy_rpc_links);

		let links = BeefyVoterLinks {
			from_block_import_justif_stream,
			to_rpc_justif_sender,
			to_rpc_best_block_sender,
		};

		let api = Arc::new(TestApi {});
		let network = peer.network_service().clone();
		let known_peers = Arc::new(Mutex::new(KnownPeers::new()));
		let gossip_validator = Arc::new(GossipValidator::new(known_peers.clone()));
		let gossip_engine =
			GossipEngine::new(network.clone(), BEEFY_PROTOCOL_NAME, gossip_validator.clone(), None);
		let worker_params = crate::worker::WorkerParams {
			client: peer.client().as_client(),
			backend: peer.client().as_backend(),
			runtime: api,
			key_store: Some(keystore).into(),
			known_peers,
			links,
			gossip_engine,
			gossip_validator,
			min_block_delta,
			metrics: None,
			network,
		};
		BeefyWorker::<_, _, _, _, _>::new(worker_params)
	}

	#[test]
	fn vote_on_min_block_delta() {
		let t = vote_target(1u32, Some(1), 1, 4);
		assert_eq!(None, t);
		let t = vote_target(2u32, Some(1), 1, 4);
		assert_eq!(None, t);
		let t = vote_target(4u32, Some(2), 1, 4);
		assert_eq!(None, t);
		let t = vote_target(6u32, Some(2), 1, 4);
		assert_eq!(Some(6), t);

		let t = vote_target(9u32, Some(4), 1, 4);
		assert_eq!(Some(8), t);

		let t = vote_target(10u32, Some(10), 1, 8);
		assert_eq!(None, t);
		let t = vote_target(12u32, Some(10), 1, 8);
		assert_eq!(None, t);
		let t = vote_target(18u32, Some(10), 1, 8);
		assert_eq!(Some(18), t);
	}

	#[test]
	fn vote_on_power_of_two() {
		let t = vote_target(1008u32, Some(1000), 1, 4);
		assert_eq!(Some(1004), t);

		let t = vote_target(1016u32, Some(1000), 1, 4);
		assert_eq!(Some(1008), t);

		let t = vote_target(1032u32, Some(1000), 1, 4);
		assert_eq!(Some(1016), t);

		let t = vote_target(1064u32, Some(1000), 1, 4);
		assert_eq!(Some(1032), t);

		let t = vote_target(1128u32, Some(1000), 1, 4);
		assert_eq!(Some(1064), t);

		let t = vote_target(1256u32, Some(1000), 1, 4);
		assert_eq!(Some(1128), t);

		let t = vote_target(1512u32, Some(1000), 1, 4);
		assert_eq!(Some(1256), t);

		let t = vote_target(1024u32, Some(1), 1, 4);
		assert_eq!(Some(513), t);
	}

	#[test]
	fn vote_on_target_block() {
		let t = vote_target(1008u32, Some(1002), 1, 4);
		assert_eq!(Some(1006), t);
		let t = vote_target(1010u32, Some(1002), 1, 4);
		assert_eq!(Some(1006), t);

		let t = vote_target(1016u32, Some(1006), 1, 4);
		assert_eq!(Some(1014), t);
		let t = vote_target(1022u32, Some(1006), 1, 4);
		assert_eq!(Some(1014), t);

		let t = vote_target(1032u32, Some(1012), 1, 4);
		assert_eq!(Some(1028), t);
		let t = vote_target(1044u32, Some(1012), 1, 4);
		assert_eq!(Some(1028), t);

		let t = vote_target(1064u32, Some(1014), 1, 4);
		assert_eq!(Some(1046), t);
		let t = vote_target(1078u32, Some(1014), 1, 4);
		assert_eq!(Some(1046), t);

		let t = vote_target(1128u32, Some(1008), 1, 4);
		assert_eq!(Some(1072), t);
		let t = vote_target(1136u32, Some(1008), 1, 4);
		assert_eq!(Some(1072), t);
	}

	#[test]
	fn vote_on_mandatory_block() {
		let t = vote_target(1008u32, Some(1002), 1004, 4);
		assert_eq!(Some(1004), t);
		let t = vote_target(1016u32, Some(1006), 1007, 4);
		assert_eq!(Some(1007), t);
		let t = vote_target(1064u32, Some(1014), 1063, 4);
		assert_eq!(Some(1063), t);
		let t = vote_target(1320u32, Some(1012), 1234, 4);
		assert_eq!(Some(1234), t);

		let t = vote_target(1128u32, Some(1008), 1008, 4);
		assert_eq!(Some(1072), t);
	}

	#[test]
	fn should_vote_target() {
		let mut oracle = VoterOracle::<Block>::new(1);

		// rounds not initialized -> should vote: `None`
		assert_eq!(oracle.voting_target(None, 1), None);

		let keys = &[Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();

		oracle.add_session(Rounds::new(1, validator_set.clone()));

		// under min delta
		oracle.min_block_delta = 4;
		assert_eq!(oracle.voting_target(Some(1), 1), None);
		assert_eq!(oracle.voting_target(Some(2), 5), None);

		// vote on min delta
		assert_eq!(oracle.voting_target(Some(4), 9), Some(8));
		oracle.min_block_delta = 8;
		assert_eq!(oracle.voting_target(Some(10), 18), Some(18));

		// vote on power of two
		oracle.min_block_delta = 1;
		assert_eq!(oracle.voting_target(Some(1000), 1008), Some(1004));
		assert_eq!(oracle.voting_target(Some(1000), 1016), Some(1008));

		// nothing new to vote on
		assert_eq!(oracle.voting_target(Some(1000), 1000), None);

		// vote on mandatory
		oracle.sessions.clear();
		oracle.add_session(Rounds::new(1000, validator_set.clone()));
		assert_eq!(oracle.voting_target(None, 1008), Some(1000));
		oracle.sessions.clear();
		oracle.add_session(Rounds::new(1001, validator_set.clone()));
		assert_eq!(oracle.voting_target(Some(1000), 1008), Some(1001));
	}

	#[test]
	fn test_oracle_accepted_interval() {
		let keys = &[Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();

		let mut oracle = VoterOracle::<Block>::new(1);

		// rounds not initialized -> should accept votes: `None`
		assert!(oracle.accepted_interval(1).is_err());

		let session_one = 1;
		oracle.add_session(Rounds::new(session_one, validator_set.clone()));
		// mandatory not done, only accept mandatory
		for i in 0..15 {
			assert_eq!(oracle.accepted_interval(i), Ok((session_one, session_one)));
		}

		// add more sessions, nothing changes
		let session_two = 11;
		let session_three = 21;
		oracle.add_session(Rounds::new(session_two, validator_set.clone()));
		oracle.add_session(Rounds::new(session_three, validator_set.clone()));
		// mandatory not done, should accept mandatory for session_one
		for i in session_three..session_three + 15 {
			assert_eq!(oracle.accepted_interval(i), Ok((session_one, session_one)));
		}

		// simulate finish mandatory for session one, prune oracle
		oracle.sessions.front_mut().unwrap().test_set_mandatory_done(true);
		oracle.try_prune();
		// session_one pruned, should accept mandatory for session_two
		for i in session_three..session_three + 15 {
			assert_eq!(oracle.accepted_interval(i), Ok((session_two, session_two)));
		}

		// simulate finish mandatory for session two, prune oracle
		oracle.sessions.front_mut().unwrap().test_set_mandatory_done(true);
		oracle.try_prune();
		// session_two pruned, should accept mandatory for session_three
		for i in session_three..session_three + 15 {
			assert_eq!(oracle.accepted_interval(i), Ok((session_three, session_three)));
		}

		// simulate finish mandatory for session three
		oracle.sessions.front_mut().unwrap().test_set_mandatory_done(true);
		// verify all other blocks in this session are now open to voting
		for i in session_three..session_three + 15 {
			assert_eq!(oracle.accepted_interval(i), Ok((session_three, i)));
		}
		// pruning does nothing in this case
		oracle.try_prune();
		for i in session_three..session_three + 15 {
			assert_eq!(oracle.accepted_interval(i), Ok((session_three, i)));
		}

		// adding new session automatically prunes "finalized" previous session
		let session_four = 31;
		oracle.add_session(Rounds::new(session_four, validator_set.clone()));
		assert_eq!(oracle.sessions.front().unwrap().session_start(), session_four);
		assert_eq!(oracle.accepted_interval(session_four + 10), Ok((session_four, session_four)));
	}

	#[test]
	fn extract_authorities_change_digest() {
		let mut header = Header::new(
			1u32.into(),
			Default::default(),
			Default::default(),
			Default::default(),
			Digest::default(),
		);

		// verify empty digest shows nothing
		assert!(find_authorities_change::<Block>(&header).is_none());

		let peers = &[Keyring::One, Keyring::Two];
		let id = 42;
		let validator_set = ValidatorSet::new(make_beefy_ids(peers), id).unwrap();
		header.digest_mut().push(DigestItem::Consensus(
			BEEFY_ENGINE_ID,
			ConsensusLog::<AuthorityId>::AuthoritiesChange(validator_set.clone()).encode(),
		));

		// verify validator set is correctly extracted from digest
		let extracted = find_authorities_change::<Block>(&header);
		assert_eq!(extracted, Some(validator_set));
	}

	#[test]
	fn extract_mmr_root_digest() {
		let mut header = Header::new(
			1u32.into(),
			Default::default(),
			Default::default(),
			Default::default(),
			Digest::default(),
		);

		// verify empty digest shows nothing
		assert!(find_mmr_root_digest::<Block>(&header).is_none());

		let mmr_root_hash = H256::random();
		header.digest_mut().push(DigestItem::Consensus(
			BEEFY_ENGINE_ID,
			ConsensusLog::<AuthorityId>::MmrRoot(mmr_root_hash).encode(),
		));

		// verify validator set is correctly extracted from digest
		let extracted = find_mmr_root_digest::<Block>(&header);
		assert_eq!(extracted, Some(mmr_root_hash));
	}

	#[test]
	fn keystore_vs_validator_set() {
		let keys = &[Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();
		let mut net = BeefyTestNet::new(1, 0);
		let mut worker = create_beefy_worker(&net.peer(0), &keys[0], 1);

		// keystore doesn't contain other keys than validators'
		assert_eq!(worker.verify_validator_set(&1, &validator_set), Ok(()));

		// unknown `Bob` key
		let keys = &[Keyring::Bob];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();
		let err_msg = "no authority public key found in store".to_string();
		let expected = Err(Error::Keystore(err_msg));
		assert_eq!(worker.verify_validator_set(&1, &validator_set), expected);

		// worker has no keystore
		worker.key_store = None.into();
		let expected_err = Err(Error::Keystore("no Keystore".into()));
		assert_eq!(worker.verify_validator_set(&1, &validator_set), expected_err);
	}

	#[test]
	fn test_finalize() {
		let keys = &[Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();
		let mut net = BeefyTestNet::new(1, 0);
		let mut worker = create_beefy_worker(&net.peer(0), &keys[0], 1);

		let (mut best_block_streams, mut finality_proofs) = get_beefy_streams(&mut net, keys);
		let mut best_block_stream = best_block_streams.drain(..).next().unwrap();
		let mut finality_proof = finality_proofs.drain(..).next().unwrap();

		let create_finality_proof = |block_num: NumberFor<Block>| {
			let commitment = Commitment {
				payload: Payload::new(known_payload_ids::MMR_ROOT_ID, vec![]),
				block_number: block_num,
				validator_set_id: validator_set.id(),
			};
			VersionedFinalityProof::V1(SignedCommitment { commitment, signatures: vec![None] })
		};

		// no 'best beefy block' or finality proofs
		assert_eq!(worker.best_beefy_block, None);
		block_on(poll_fn(move |cx| {
			assert_eq!(best_block_stream.poll_next_unpin(cx), Poll::Pending);
			assert_eq!(finality_proof.poll_next_unpin(cx), Poll::Pending);
			Poll::Ready(())
		}));

		// unknown hash for block #1
		let (mut best_block_streams, mut finality_proofs) = get_beefy_streams(&mut net, keys);
		let mut best_block_stream = best_block_streams.drain(..).next().unwrap();
		let mut finality_proof = finality_proofs.drain(..).next().unwrap();
		let justif = create_finality_proof(1);
		worker.finalize(justif.clone());
		assert_eq!(worker.best_beefy_block, Some(1));
		block_on(poll_fn(move |cx| {
			assert_eq!(best_block_stream.poll_next_unpin(cx), Poll::Pending);
			match finality_proof.poll_next_unpin(cx) {
				// expect justification
				Poll::Ready(Some(received)) => assert_eq!(received, justif),
				v => panic!("unexpected value: {:?}", v),
			}
			Poll::Ready(())
		}));

		// generate 2 blocks, try again expect success
		let (mut best_block_streams, _) = get_beefy_streams(&mut net, keys);
		let mut best_block_stream = best_block_streams.drain(..).next().unwrap();
		net.generate_blocks(2, 10, &validator_set, false);

		let justif = create_finality_proof(2);
		worker.finalize(justif);
		assert_eq!(worker.best_beefy_block, Some(2));
		block_on(poll_fn(move |cx| {
			match best_block_stream.poll_next_unpin(cx) {
				// expect Some(hash-of-block-2)
				Poll::Ready(Some(hash)) => {
					let block_num = net.peer(0).client().as_client().number(hash).unwrap();
					assert_eq!(block_num, Some(2));
				},
				v => panic!("unexpected value: {:?}", v),
			}
			Poll::Ready(())
		}));
	}

	#[test]
	fn should_init_session() {
		let keys = &[Keyring::Alice];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();
		let mut net = BeefyTestNet::new(1, 0);
		let mut worker = create_beefy_worker(&net.peer(0), &keys[0], 1);

		assert!(worker.voting_oracle.sessions.is_empty());

		worker.init_session_at(validator_set.clone(), 1);
		let worker_rounds = worker.voting_oracle.rounds_mut().unwrap();
		assert_eq!(worker_rounds.session_start(), 1);
		assert_eq!(worker_rounds.validators(), validator_set.validators());
		assert_eq!(worker_rounds.validator_set_id(), validator_set.id());

		// new validator set
		let keys = &[Keyring::Bob];
		let new_validator_set = ValidatorSet::new(make_beefy_ids(keys), 1).unwrap();

		worker.init_session_at(new_validator_set.clone(), 11);
		// Since mandatory is not done for old rounds, we still get those.
		let rounds = worker.voting_oracle.rounds_mut().unwrap();
		assert_eq!(rounds.validator_set_id(), validator_set.id());
		// Let's finalize mandatory.
		rounds.test_set_mandatory_done(true);
		worker.voting_oracle.try_prune();
		// Now we should get the next round.
		let rounds = worker.voting_oracle.rounds_mut().unwrap();
		// Expect new values.
		assert_eq!(rounds.session_start(), 11);
		assert_eq!(rounds.validators(), new_validator_set.validators());
		assert_eq!(rounds.validator_set_id(), new_validator_set.id());
	}

	#[test]
	fn should_triage_votes_and_process_later() {
		let keys = &[Keyring::Alice, Keyring::Bob];
		let validator_set = ValidatorSet::new(make_beefy_ids(keys), 0).unwrap();
		let mut net = BeefyTestNet::new(1, 0);
		let mut worker = create_beefy_worker(&net.peer(0), &keys[0], 1);

		fn new_vote(
			block_number: NumberFor<Block>,
		) -> VoteMessage<NumberFor<Block>, AuthorityId, Signature> {
			let commitment = Commitment {
				payload: Payload::new(*b"BF", vec![]),
				block_number,
				validator_set_id: 0,
			};
			VoteMessage {
				commitment,
				id: Keyring::Alice.public(),
				signature: Keyring::Alice.sign(b"I am committed"),
			}
		}

		// best grandpa is 20
		let best_grandpa_header = Header::new(
			20u32.into(),
			Default::default(),
			Default::default(),
			Default::default(),
			Digest::default(),
		);

		worker.voting_oracle.add_session(Rounds::new(10, validator_set.clone()));
		worker.best_grandpa_block_header = best_grandpa_header;

		// triage votes for blocks 10..13
		worker.triage_incoming_vote(new_vote(10)).unwrap();
		worker.triage_incoming_vote(new_vote(11)).unwrap();
		worker.triage_incoming_vote(new_vote(12)).unwrap();
		// triage votes for blocks 20..23
		worker.triage_incoming_vote(new_vote(20)).unwrap();
		worker.triage_incoming_vote(new_vote(21)).unwrap();
		worker.triage_incoming_vote(new_vote(22)).unwrap();

		// vote for 10 should have been handled, while the rest buffered for later processing
		let mut votes = worker.pending_votes.values();
		assert_eq!(votes.next().unwrap().first().unwrap().commitment.block_number, 11);
		assert_eq!(votes.next().unwrap().first().unwrap().commitment.block_number, 12);
		assert_eq!(votes.next().unwrap().first().unwrap().commitment.block_number, 20);
		assert_eq!(votes.next().unwrap().first().unwrap().commitment.block_number, 21);
		assert_eq!(votes.next().unwrap().first().unwrap().commitment.block_number, 22);
		assert!(votes.next().is_none());

		// simulate mandatory done, and retry buffered votes
		worker.voting_oracle.rounds_mut().unwrap().test_set_mandatory_done(true);
		worker.try_pending_justif_and_votes().unwrap();
		// all blocks <= grandpa finalized should have been handled, rest still buffered
		let mut votes = worker.pending_votes.values();
		assert_eq!(votes.next().unwrap().first().unwrap().commitment.block_number, 21);
		assert_eq!(votes.next().unwrap().first().unwrap().commitment.block_number, 22);
	}
}
