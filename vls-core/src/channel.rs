use core::any::Any;
use core::fmt;
use core::fmt::{Debug, Error, Formatter};

use bitcoin::hashes::hex::{self, FromHex};
use bitcoin::hashes::sha256d::Hash as Sha256dHash;
use bitcoin::hashes::Hash;
use bitcoin::secp256k1::{self, ecdsa::Signature, All, Message, PublicKey, Secp256k1, SecretKey};
use bitcoin::util::sighash::SighashCache;
use bitcoin::{EcdsaSighashType, Network, OutPoint, Script, Transaction};
use lightning::chain;
use lightning::chain::keysinterface::{
    ChannelSigner, EcdsaChannelSigner, EntropySource, InMemorySigner, SignerProvider,
};
use lightning::ln::chan_utils::{
    build_htlc_transaction, derive_private_key, derive_public_revocation_key,
    get_htlc_redeemscript, make_funding_redeemscript, ChannelPublicKeys,
    ChannelTransactionParameters, ClosingTransaction, CommitmentTransaction,
    CounterpartyChannelTransactionParameters, HTLCOutputInCommitment, HolderCommitmentTransaction,
    TxCreationKeys,
};
use lightning::ln::{chan_utils, PaymentHash, PaymentPreimage};
use log::*;
use serde_derive::{Deserialize, Serialize};
use serde_with::serde_as;

use crate::monitor::ChainMonitor;
use crate::node::{Node, RoutedPayment};
use crate::policy::error::policy_error;
use crate::policy::validator::{ChainState, CommitmentSignatures, EnforcementState, Validator};
use crate::prelude::*;
use crate::tx::tx::{CommitmentInfo2, HTLCInfo2};
use crate::util::crypto_utils::derive_public_key;
use crate::util::debug_utils::{DebugHTLCOutputInCommitment, DebugInMemorySigner, DebugVecVecU8};
use crate::util::ser_util::{ChannelPublicKeysDef, OutPointDef, ScriptDef};
use crate::util::status::{internal_error, invalid_argument, Status};
use crate::util::transaction_utils::add_holder_sig;
use crate::util::INITIAL_COMMITMENT_NUMBER;
use crate::wallet::Wallet;
use crate::{policy_err, Arc, Weak};

/// Channel identifier
///
/// This ID is not related to the channel IDs in the Lightning protocol.
///
/// A channel may have more than one ID.
///
/// The channel keys are derived from this and a base key.
#[derive(PartialEq, Eq, Clone, PartialOrd, Ord)]
pub struct ChannelId(Vec<u8>);

impl ChannelId {
    /// Create an ID
    pub fn new(inner: &[u8]) -> Self {
        Self(inner.to_vec())
    }

    /// Convert to a byte slice
    pub fn as_slice(&self) -> &[u8] {
        self.0.as_slice()
    }

    /// Get a reference to the byte vector
    pub fn inner(&self) -> &Vec<u8> {
        &self.0
    }
}

impl Debug for ChannelId {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), Error> {
        hex::format_hex(&self.0, f)
    }
}

impl fmt::Display for ChannelId {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        hex::format_hex(&self.0, f)
    }
}

/// Bitcoin Signature which specifies EcdsaSighashType
#[derive(Debug)]
pub struct TypedSignature {
    /// The signature
    pub sig: Signature,
    /// The sighash type
    pub typ: EcdsaSighashType,
}

impl TypedSignature {
    /// Serialize the signature and append the sighash type byte.
    pub fn serialize(&self) -> Vec<u8> {
        let mut ss = self.sig.serialize_der().to_vec();
        ss.push(self.typ as u8);
        ss
    }

    /// A TypedSignature with SIGHASH_ALL
    pub fn all(sig: Signature) -> Self {
        Self { sig, typ: EcdsaSighashType::All }
    }
}

/// The commitment type, based on the negotiated option
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub enum CommitmentType {
    /// No longer used - dynamic to-remote key
    /// DEPRECATED
    Legacy,
    /// Static to-remote key
    StaticRemoteKey,
    /// Anchors
    /// DEPRECATED
    Anchors,
    /// Anchors, zero fee htlc
    AnchorsZeroFeeHtlc,
}

/// The negotiated parameters for the [Channel]
#[serde_as]
#[derive(Clone, PartialEq, Serialize, Deserialize)]
pub struct ChannelSetup {
    /// Whether the channel is outbound
    pub is_outbound: bool,
    /// The total the channel was funded with
    pub channel_value_sat: u64,
    // DUP keys.inner.channel_value_satoshis
    /// How much was pushed to the counterparty
    pub push_value_msat: u64,
    /// The funding outpoint
    #[serde_as(as = "OutPointDef")]
    pub funding_outpoint: OutPoint,
    /// locally imposed requirement on the remote commitment transaction to_self_delay
    pub holder_selected_contest_delay: u16,
    /// The holder's optional upfront shutdown script
    #[serde_as(as = "Option<ScriptDef>")]
    pub holder_shutdown_script: Option<Script>,
    /// The counterparty's basepoints and pubkeys
    #[serde(with = "ChannelPublicKeysDef")]
    pub counterparty_points: ChannelPublicKeys,
    // DUP keys.inner.remote_channel_pubkeys
    /// remotely imposed requirement on the local commitment transaction to_self_delay
    pub counterparty_selected_contest_delay: u16,
    /// The counterparty's optional upfront shutdown script
    #[serde_as(as = "Option<ScriptDef>")]
    pub counterparty_shutdown_script: Option<Script>,
    /// The negotiated commitment type
    pub commitment_type: CommitmentType,
}

// Need to define manually because ChannelPublicKeys doesn't derive Debug.
// kcov-ignore-start
impl fmt::Debug for ChannelSetup {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelSetup")
            .field("is_outbound", &self.is_outbound)
            .field("channel_value_sat", &self.channel_value_sat)
            .field("push_value_msat", &self.push_value_msat)
            .field("funding_outpoint", &self.funding_outpoint)
            .field("holder_selected_contest_delay", &self.holder_selected_contest_delay)
            .field("holder_shutdown_script", &self.holder_shutdown_script)
            .field("counterparty_points", log_channel_public_keys!(&self.counterparty_points))
            .field("counterparty_selected_contest_delay", &self.counterparty_selected_contest_delay)
            .field("counterparty_shutdown_script", &self.counterparty_shutdown_script)
            .field("commitment_type", &self.commitment_type)
            .finish()
    }
}
// kcov-ignore-end

impl ChannelSetup {
    /// True if this channel uses static to_remote key
    pub fn is_static_remotekey(&self) -> bool {
        self.commitment_type != CommitmentType::Legacy
    }

    /// True if this channel uses anchors
    pub fn is_anchors(&self) -> bool {
        self.commitment_type == CommitmentType::Anchors
            || self.commitment_type == CommitmentType::AnchorsZeroFeeHtlc
    }

    /// True if this channel uses zero fee htlc with anchors
    pub fn is_zero_fee_htlc(&self) -> bool {
        self.commitment_type == CommitmentType::AnchorsZeroFeeHtlc
    }
}

/// A trait implemented by both channel states.  See [ChannelSlot]
pub trait ChannelBase: Any {
    /// Get the channel basepoints and public keys
    fn get_channel_basepoints(&self) -> ChannelPublicKeys;
    /// Get the per-commitment point for a holder commitment transaction
    fn get_per_commitment_point(&self, commitment_number: u64) -> Result<PublicKey, Status>;
    /// Get the per-commitment secret for a holder commitment transaction
    // TODO leaking secret
    fn get_per_commitment_secret(&self, commitment_number: u64) -> Result<SecretKey, Status>;
    /// Check a future secret to support `option_data_loss_protect`
    fn check_future_secret(&self, commit_num: u64, suggested: &SecretKey) -> Result<bool, Status>;
    /// Returns the validator for this channel
    fn validator(&self) -> Arc<dyn Validator>;

    // TODO remove when LDK workaround is removed in LoopbackSigner
    #[allow(missing_docs)]
    #[cfg(any(test, feature = "test_utils"))]
    fn set_next_holder_commit_num_for_testing(&mut self, _num: u64) {
        // Do nothing for ChannelStub.  Channel will override.
    }
}

/// A channel can be in two states - before [Node::ready_channel] it's a
/// [ChannelStub], afterwards it's a [Channel].  This enum keeps track
/// of the two different states.
#[derive(Debug, Clone)]
pub enum ChannelSlot {
    /// Initial state, not ready
    Stub(ChannelStub),
    /// Ready after negotiation is complete
    Ready(Channel),
}

impl ChannelSlot {
    /// The initial channel ID
    pub fn id(&self) -> ChannelId {
        match self {
            ChannelSlot::Stub(stub) => stub.id0.clone(),
            ChannelSlot::Ready(chan) => chan.id0.clone(),
        }
    }

    /// The basepoints
    pub fn get_channel_basepoints(&self) -> ChannelPublicKeys {
        match self {
            ChannelSlot::Stub(stub) => stub.get_channel_basepoints(),
            ChannelSlot::Ready(chan) => chan.get_channel_basepoints(),
        }
    }

    /// Assume this is a channel stub, and return it.
    /// Panics if it's not.
    pub fn unwrap_stub(&self) -> &ChannelStub {
        match self {
            ChannelSlot::Stub(stub) => stub,
            ChannelSlot::Ready(_) => panic!("unwrap_stub called on ChannelSlot::Ready"),
        }
    }
}

/// A channel takes this form after [Node::new_channel], and before [Node::ready_channel]
#[derive(Clone)]
pub struct ChannelStub {
    /// A backpointer to the node
    pub node: Weak<Node>,
    pub(crate) secp_ctx: Secp256k1<All>,
    /// The signer for this channel
    pub keys: InMemorySigner,
    // Incomplete, channel_value_sat is placeholder.
    /// The initial channel ID, used to find the channel in the node
    pub id0: ChannelId,
    /// Blockheight when created
    pub blockheight: u32,
}

// Need to define manually because InMemorySigner doesn't derive Debug.
// kcov-ignore-start
impl fmt::Debug for ChannelStub {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ChannelStub")
            .field("keys", &DebugInMemorySigner(&self.keys))
            .field("id0", &self.id0)
            .finish()
    }
}
// kcov-ignore-end

impl ChannelBase for ChannelStub {
    fn get_channel_basepoints(&self) -> ChannelPublicKeys {
        self.keys.pubkeys().clone()
    }

    fn get_per_commitment_point(&self, commitment_number: u64) -> Result<PublicKey, Status> {
        if commitment_number != 0 {
            return Err(policy_error(format!(
                "channel stub can only return point for commitment number zero",
            ))
            .into());
        }
        Ok(self.keys.get_per_commitment_point(
            INITIAL_COMMITMENT_NUMBER - commitment_number,
            &self.secp_ctx,
        ))
    }

    fn get_per_commitment_secret(&self, _commitment_number: u64) -> Result<SecretKey, Status> {
        // We can't release a commitment_secret from a ChannelStub ever.
        Err(policy_error(format!("channel stub cannot release commitment secret")).into())
    }

    fn check_future_secret(
        &self,
        commitment_number: u64,
        suggested: &SecretKey,
    ) -> Result<bool, Status> {
        let secret_data =
            self.keys.release_commitment_secret(INITIAL_COMMITMENT_NUMBER - commitment_number);
        Ok(suggested[..] == secret_data)
    }

    fn validator(&self) -> Arc<dyn Validator> {
        let node = self.node.upgrade().unwrap();
        let v = node.validator_factory.lock().unwrap().make_validator(
            node.network(),
            node.get_id(),
            Some(self.id0.clone()),
        );
        v
    }
}

impl ChannelStub {
    pub(crate) fn channel_keys_with_channel_value(&self, channel_value_sat: u64) -> InMemorySigner {
        let secp_ctx = Secp256k1::signing_only();
        let keys = &self.keys;
        InMemorySigner::new(
            &secp_ctx,
            keys.funding_key,
            keys.revocation_base_key,
            keys.payment_key,
            keys.delayed_payment_base_key,
            keys.htlc_base_key,
            keys.commitment_seed,
            channel_value_sat,
            keys.channel_keys_id(),
            keys.get_secure_random_bytes(),
        )
    }
}

/// After [Node::ready_channel]
#[derive(Clone)]
pub struct Channel {
    /// A backpointer to the node
    pub node: Weak<Node>,
    /// The logger
    pub(crate) secp_ctx: Secp256k1<All>,
    /// The signer for this channel
    pub keys: InMemorySigner,
    /// Channel state for policy enforcement purposes
    pub enforcement_state: EnforcementState,
    /// The negotiated channel setup
    pub setup: ChannelSetup,
    /// The initial channel ID
    pub id0: ChannelId,
    /// The optional permanent channel ID
    pub id: Option<ChannelId>,
    /// The chain monitor
    pub monitor: ChainMonitor,
}

impl Debug for Channel {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.write_str("channel")
    }
}

impl ChannelBase for Channel {
    // TODO move out to impl Channel {} once LDK workaround is removed
    #[cfg(any(test, feature = "test_utils"))]
    fn set_next_holder_commit_num_for_testing(&mut self, num: u64) {
        self.enforcement_state.set_next_holder_commit_num_for_testing(num);
    }

    fn get_channel_basepoints(&self) -> ChannelPublicKeys {
        self.keys.pubkeys().clone()
    }

    fn get_per_commitment_point(&self, commitment_number: u64) -> Result<PublicKey, Status> {
        let next_holder_commit_num = self.enforcement_state.next_holder_commit_num;
        // The following check is relaxed by +1 because LDK fetches the next commitment point
        // before it calls validate_holder_commitment_tx.
        if commitment_number > next_holder_commit_num + 1 {
            return Err(policy_error(format!(
                "get_per_commitment_point: \
                 commitment_number {} invalid when next_holder_commit_num is {}",
                commitment_number, next_holder_commit_num,
            ))
            .into());
        }
        Ok(self.keys.get_per_commitment_point(
            INITIAL_COMMITMENT_NUMBER - commitment_number,
            &self.secp_ctx,
        ))
    }

    fn get_per_commitment_secret(&self, commitment_number: u64) -> Result<SecretKey, Status> {
        let next_holder_commit_num = self.enforcement_state.next_holder_commit_num;
        // When we try to release commitment number n, we must have a signature
        // for commitment number `n+1`, so `next_holder_commit_num` must be at least
        // `n+2`.
        // Also, given that we previous validated the commitment tx when we
        // got the counterparty signature for it, then we must have fulfilled
        // policy-revoke-new-commitment-valid
        if commitment_number + 2 > next_holder_commit_num {
            let validator = self.validator();
            policy_err!(
                validator,
                "policy-revoke-new-commitment-signed",
                "cannot revoke commitment_number {} when next_holder_commit_num is {}",
                commitment_number,
                next_holder_commit_num,
            )
        }
        let secret =
            self.keys.release_commitment_secret(INITIAL_COMMITMENT_NUMBER - commitment_number);
        Ok(SecretKey::from_slice(&secret).unwrap())
    }

    fn check_future_secret(
        &self,
        commitment_number: u64,
        suggested: &SecretKey,
    ) -> Result<bool, Status> {
        let secret_data =
            self.keys.release_commitment_secret(INITIAL_COMMITMENT_NUMBER - commitment_number);
        Ok(suggested[..] == secret_data)
    }

    fn validator(&self) -> Arc<dyn Validator> {
        let node = self.get_node();
        let v = node.validator_factory.lock().unwrap().make_validator(
            self.network(),
            node.get_id(),
            Some(self.id0.clone()),
        );
        v
    }
}

impl Channel {
    /// The channel ID
    pub fn id(&self) -> ChannelId {
        self.id.clone().unwrap_or(self.id0.clone())
    }

    #[allow(missing_docs)]
    #[cfg(any(test, feature = "test_utils"))]
    pub fn set_next_counterparty_commit_num_for_testing(
        &mut self,
        num: u64,
        current_point: PublicKey,
    ) {
        self.enforcement_state.set_next_counterparty_commit_num_for_testing(num, current_point);
    }

    #[allow(missing_docs)]
    #[cfg(any(test, feature = "test_utils"))]
    pub fn set_next_counterparty_revoke_num_for_testing(&mut self, num: u64) {
        self.enforcement_state.set_next_counterparty_revoke_num_for_testing(num);
    }

    pub(crate) fn get_chain_state(&self) -> ChainState {
        self.monitor.as_chain_state()
    }
}

// Phase 2
impl Channel {
    // Phase 2
    /// Public for testing purposes
    pub fn make_counterparty_tx_keys(
        &self,
        per_commitment_point: &PublicKey,
    ) -> Result<TxCreationKeys, Status> {
        let holder_points = self.keys.pubkeys();

        let counterparty_points = self.keys.counterparty_pubkeys();

        Ok(self.make_tx_keys(per_commitment_point, counterparty_points, holder_points))
    }

    pub(crate) fn make_holder_tx_keys(
        &self,
        per_commitment_point: &PublicKey,
    ) -> Result<TxCreationKeys, Status> {
        let holder_points = self.keys.pubkeys();

        let counterparty_points = self.keys.counterparty_pubkeys();

        Ok(self.make_tx_keys(per_commitment_point, holder_points, counterparty_points))
    }

    fn make_tx_keys(
        &self,
        per_commitment_point: &PublicKey,
        a_points: &ChannelPublicKeys,
        b_points: &ChannelPublicKeys,
    ) -> TxCreationKeys {
        TxCreationKeys::derive_new(
            &self.secp_ctx,
            &per_commitment_point,
            &a_points.delayed_payment_basepoint,
            &a_points.htlc_basepoint,
            &b_points.revocation_basepoint,
            &b_points.htlc_basepoint,
        )
    }

    fn derive_counterparty_payment_pubkey(
        &self,
        remote_per_commitment_point: &PublicKey,
    ) -> Result<PublicKey, Status> {
        let holder_points = self.keys.pubkeys();
        let counterparty_key = if self.setup.is_static_remotekey() {
            holder_points.payment_point
        } else {
            derive_public_key(
                &self.secp_ctx,
                &remote_per_commitment_point,
                &holder_points.payment_point,
            )
            .map_err(|err| internal_error(format!("could not derive counterparty_key: {}", err)))?
        };
        Ok(counterparty_key)
    }

    /// Sign a counterparty commitment transaction after rebuilding it
    /// from the supplied arguments.
    // TODO anchors support once LDK supports it
    pub fn sign_counterparty_commitment_tx_phase2(
        &mut self,
        remote_per_commitment_point: &PublicKey,
        commitment_number: u64,
        feerate_per_kw: u32,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
    ) -> Result<(Signature, Vec<Signature>), Status> {
        // Since we didn't have the value at the real open, validate it now.
        let validator = self.validator();
        validator.validate_channel_value(&self.setup)?;

        let info2 = self.build_counterparty_commitment_info(
            remote_per_commitment_point,
            to_holder_value_sat,
            to_counterparty_value_sat,
            offered_htlcs.clone(),
            received_htlcs.clone(),
            feerate_per_kw,
        )?;

        let node = self.get_node();
        let mut state = node.get_state();
        let delta =
            self.enforcement_state.claimable_balances(&*state, None, Some(&info2), &self.setup);

        let incoming_payment_summary =
            self.enforcement_state.incoming_payments_summary(None, Some(&info2));

        validator.validate_counterparty_commitment_tx(
            &self.enforcement_state,
            commitment_number,
            &remote_per_commitment_point,
            &self.setup,
            &self.get_chain_state(),
            &info2,
        )?;

        let htlcs = Self::htlcs_info2_to_oic(offered_htlcs, received_htlcs);

        // since we independently re-create the tx, this also performs the
        // policy-commitment-* controls
        let commitment_tx = self.make_counterparty_commitment_tx(
            remote_per_commitment_point,
            commitment_number,
            feerate_per_kw,
            to_holder_value_sat,
            to_counterparty_value_sat,
            htlcs,
        );

        let (sig, htlc_sigs) = self
            .keys
            .sign_counterparty_commitment(&commitment_tx, Vec::new(), &self.secp_ctx)
            .map_err(|_| internal_error("failed to sign"))?;

        let outgoing_payment_summary = self.enforcement_state.payments_summary(None, Some(&info2));
        state.validate_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator.clone(),
        )?;

        // Only advance the state if nothing goes wrong.
        validator.set_next_counterparty_commit_num(
            &mut self.enforcement_state,
            commitment_number + 1,
            remote_per_commitment_point.clone(),
            info2,
        )?;

        state.apply_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator,
        );

        trace_enforcement_state!(self);
        self.persist()?;
        Ok((sig, htlc_sigs))
    }

    // restore node state payments from the current commitment transactions
    pub(crate) fn restore_payments(&self) {
        let node = self.get_node();

        let incoming_payment_summary = self.enforcement_state.incoming_payments_summary(None, None);
        let outgoing_payment_summary = self.enforcement_state.payments_summary(None, None);

        let mut hashes: UnorderedSet<&PaymentHash> = UnorderedSet::new();
        hashes.extend(incoming_payment_summary.keys());
        hashes.extend(outgoing_payment_summary.keys());

        let mut state = node.get_state();

        for hash in hashes {
            let payment = state.payments.entry(*hash).or_insert_with(|| RoutedPayment::new());
            let incoming_sat = incoming_payment_summary.get(hash).map(|a| *a).unwrap_or(0);
            let outgoing_sat = outgoing_payment_summary.get(hash).map(|a| *a).unwrap_or(0);
            payment.apply(&self.id0, incoming_sat, outgoing_sat);
        }
    }

    // This function is needed for testing with mutated keys.
    /// Public for testing purposes
    pub fn make_counterparty_commitment_tx_with_keys(
        &self,
        keys: TxCreationKeys,
        commitment_number: u64,
        feerate_per_kw: u32,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        htlcs: Vec<HTLCOutputInCommitment>,
    ) -> CommitmentTransaction {
        let mut htlcs_with_aux = htlcs.iter().map(|h| (h.clone(), ())).collect();
        let channel_parameters = self.make_channel_parameters();
        let parameters = channel_parameters.as_counterparty_broadcastable();
        let commitment_tx = CommitmentTransaction::new_with_auxiliary_htlc_data(
            INITIAL_COMMITMENT_NUMBER - commitment_number,
            to_counterparty_value_sat,
            to_holder_value_sat,
            self.setup.is_anchors(),
            self.keys.counterparty_pubkeys().funding_pubkey,
            self.keys.pubkeys().funding_pubkey,
            keys,
            feerate_per_kw,
            &mut htlcs_with_aux,
            &parameters,
        );
        commitment_tx
    }

    /// Public for testing purposes
    pub fn make_counterparty_commitment_tx(
        &self,
        remote_per_commitment_point: &PublicKey,
        commitment_number: u64,
        feerate_per_kw: u32,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        htlcs: Vec<HTLCOutputInCommitment>,
    ) -> CommitmentTransaction {
        let keys = self.make_counterparty_tx_keys(remote_per_commitment_point).unwrap();
        self.make_counterparty_commitment_tx_with_keys(
            keys,
            commitment_number,
            feerate_per_kw,
            to_holder_value_sat,
            to_counterparty_value_sat,
            htlcs,
        )
    }

    fn check_holder_tx_signatures(
        &self,
        per_commitment_point: &PublicKey,
        txkeys: &TxCreationKeys,
        feerate_per_kw: u32,
        counterparty_commit_sig: &Signature,
        counterparty_htlc_sigs: &[Signature],
        recomposed_tx: CommitmentTransaction,
    ) -> Result<(), Status> {
        let redeemscript = make_funding_redeemscript(
            &self.keys.pubkeys().funding_pubkey,
            &self.setup.counterparty_points.funding_pubkey,
        );

        let sighash = Message::from_slice(
            &SighashCache::new(&recomposed_tx.trust().built_transaction().transaction)
                .segwit_signature_hash(
                    0,
                    &redeemscript,
                    self.setup.channel_value_sat,
                    EcdsaSighashType::All,
                )
                .unwrap()[..],
        )
        .map_err(|ve| internal_error(format!("sighash failed: {}", ve)))?;

        self.secp_ctx
            .verify_ecdsa(
                &sighash,
                &counterparty_commit_sig,
                &self.setup.counterparty_points.funding_pubkey,
            )
            .map_err(|ve| policy_error(format!("commit sig verify failed: {}", ve)))?;

        let commitment_txid = recomposed_tx.trust().txid();
        let to_self_delay = self.setup.counterparty_selected_contest_delay;

        let htlc_pubkey = derive_public_key(
            &self.secp_ctx,
            &per_commitment_point,
            &self.keys.counterparty_pubkeys().htlc_basepoint,
        )
        .map_err(|err| internal_error(format!("derive_public_key failed: {}", err)))?;

        let sig_hash_type = if self.setup.is_anchors() {
            EcdsaSighashType::SinglePlusAnyoneCanPay
        } else {
            EcdsaSighashType::All
        };

        let build_feerate = if self.setup.is_zero_fee_htlc() { 0 } else { feerate_per_kw };

        for ndx in 0..recomposed_tx.htlcs().len() {
            let htlc = &recomposed_tx.htlcs()[ndx];

            let htlc_redeemscript = get_htlc_redeemscript(htlc, self.setup.is_anchors(), &txkeys);

            // policy-onchain-format-standard
            let recomposed_htlc_tx = build_htlc_transaction(
                &commitment_txid,
                build_feerate,
                to_self_delay,
                htlc,
                self.setup.is_anchors(),
                !self.setup.is_zero_fee_htlc(),
                &txkeys.broadcaster_delayed_payment_key,
                &txkeys.revocation_key,
            );

            let recomposed_tx_sighash = Message::from_slice(
                &SighashCache::new(&recomposed_htlc_tx)
                    .segwit_signature_hash(
                        0,
                        &htlc_redeemscript,
                        htlc.amount_msat / 1000,
                        sig_hash_type,
                    )
                    .unwrap()[..],
            )
            .map_err(|err| invalid_argument(format!("sighash failed for htlc {}: {}", ndx, err)))?;

            self.secp_ctx
                .verify_ecdsa(&recomposed_tx_sighash, &counterparty_htlc_sigs[ndx], &htlc_pubkey)
                .map_err(|err| {
                    policy_error(format!("commit sig verify failed for htlc {}: {}", ndx, err))
                })?;
        }
        Ok(())
    }

    fn advance_holder_commitment_state(
        &mut self,
        validator: Arc<dyn Validator>,
        commitment_number: u64,
        info2: CommitmentInfo2,
        counterparty_signatures: CommitmentSignatures,
    ) -> Result<(PublicKey, Option<SecretKey>), Status> {
        // Advance the local commitment number state.
        validator.set_next_holder_commit_num(
            &mut self.enforcement_state,
            commitment_number + 1,
            info2,
            counterparty_signatures,
        )?;

        // These calls are guaranteed to pass the commitment_number
        // check because we just advanced it to the right spot above.
        let next_holder_commitment_point =
            self.get_per_commitment_point(commitment_number + 1).unwrap();
        let maybe_old_secret = if commitment_number >= 1 {
            Some(self.get_per_commitment_secret(commitment_number - 1).unwrap())
        } else {
            None
        };
        Ok((next_holder_commitment_point, maybe_old_secret))
    }

    /// Validate the counterparty's signatures on the holder's
    /// commitment and HTLCs when the commitment_signed message is
    /// received.  Returns the next per_commitment_point and the
    /// holder's revocation secret for the prior commitment.  This
    /// method advances the expected next holder commitment number in
    /// the signer's state.
    pub fn validate_holder_commitment_tx_phase2(
        &mut self,
        commitment_number: u64,
        feerate_per_kw: u32,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
        counterparty_commit_sig: &Signature,
        counterparty_htlc_sigs: &[Signature],
    ) -> Result<(PublicKey, Option<SecretKey>), Status> {
        let per_commitment_point = &self.get_per_commitment_point(commitment_number)?;
        let info2 = self.build_holder_commitment_info(
            &per_commitment_point,
            to_holder_value_sat,
            to_counterparty_value_sat,
            offered_htlcs,
            received_htlcs,
            feerate_per_kw,
        )?;

        let node = self.get_node();
        let mut state = node.get_state();
        let delta =
            self.enforcement_state.claimable_balances(&*state, Some(&info2), None, &self.setup);

        let incoming_payment_summary =
            self.enforcement_state.incoming_payments_summary(Some(&info2), None);

        let validator = self.validator();
        validator
            .validate_holder_commitment_tx(
                &self.enforcement_state,
                commitment_number,
                &per_commitment_point,
                &self.setup,
                &self.get_chain_state(),
                &info2,
            )
            .map_err(|ve| {
                warn!(
                    "VALIDATION FAILED: {}\nsetup={:#?}\nstate={:#?}\ninfo={:#?}",
                    ve,
                    &self.setup,
                    &self.get_chain_state(),
                    &info2,
                );
                ve
            })?;

        let htlcs =
            Self::htlcs_info2_to_oic(info2.offered_htlcs.clone(), info2.received_htlcs.clone());

        let txkeys = self.make_holder_tx_keys(&per_commitment_point).unwrap();
        // policy-commitment-*
        let recomposed_tx = self.make_holder_commitment_tx(
            commitment_number,
            &txkeys,
            feerate_per_kw,
            to_holder_value_sat,
            to_counterparty_value_sat,
            htlcs.clone(),
        );

        self.check_holder_tx_signatures(
            &per_commitment_point,
            &txkeys,
            feerate_per_kw,
            counterparty_commit_sig,
            counterparty_htlc_sigs,
            recomposed_tx,
        )?;

        let outgoing_payment_summary = self.enforcement_state.payments_summary(Some(&info2), None);
        state.validate_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator.clone(),
        )?;

        let counterparty_signatures =
            CommitmentSignatures(counterparty_commit_sig.clone(), counterparty_htlc_sigs.to_vec());
        let (next_holder_commitment_point, maybe_old_secret) = self
            .advance_holder_commitment_state(
                validator.clone(),
                commitment_number,
                info2,
                counterparty_signatures,
            )?;

        state.apply_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator,
        );

        trace_enforcement_state!(self);
        self.persist()?;

        Ok((next_holder_commitment_point, maybe_old_secret))
    }

    /// Sign a holder commitment when force-closing
    pub fn sign_holder_commitment_tx_phase2(
        &mut self,
        commitment_number: u64,
    ) -> Result<(Signature, Vec<Signature>), Status> {
        // We are just signing the latest commitment info that we previously
        // stored in the enforcement state, while checking that the commitment
        // number supplied by the caller matches the one we expect.
        //
        // policy-commitment-holder-not-revoked is implicitly checked by
        // the fact that the current holder commitment info in the enforcement
        // state cannot have been revoked.
        let validator = self.validator();
        let info2 = validator
            .get_current_holder_commitment_info(&mut self.enforcement_state, commitment_number)?;

        let htlcs =
            Self::htlcs_info2_to_oic(info2.offered_htlcs.clone(), info2.received_htlcs.clone());
        let per_commitment_point = self.get_per_commitment_point(commitment_number)?;

        let build_feerate = if self.setup.is_zero_fee_htlc() { 0 } else { info2.feerate_per_kw };
        let txkeys = self.make_holder_tx_keys(&per_commitment_point).unwrap();
        // policy-onchain-format-standard
        let recomposed_tx = self.make_holder_commitment_tx(
            commitment_number,
            &txkeys,
            build_feerate,
            info2.to_broadcaster_value_sat,
            info2.to_countersigner_value_sat,
            htlcs,
        );

        // We provide a dummy signature for the remote, since we don't require that sig
        // to be passed in to this call.  It would have been better if HolderCommitmentTransaction
        // didn't require the remote sig.
        // TODO consider if we actually want the sig for policy checks
        let htlcs_len = recomposed_tx.htlcs().len();
        let mut htlc_dummy_sigs = Vec::with_capacity(htlcs_len);
        htlc_dummy_sigs.resize(htlcs_len, Self::dummy_sig());

        // Holder commitments need an extra wrapper for the LDK signature routine.
        let recomposed_holder_tx = HolderCommitmentTransaction::new(
            recomposed_tx,
            Self::dummy_sig(),
            htlc_dummy_sigs,
            &self.keys.pubkeys().funding_pubkey,
            &self.keys.counterparty_pubkeys().funding_pubkey,
        );

        // Sign the recomposed commitment.
        let (sig, htlc_sigs) = self
            .keys
            .sign_holder_commitment_and_htlcs(&recomposed_holder_tx, &self.secp_ctx)
            .map_err(|_| internal_error("failed to sign"))?;

        self.enforcement_state.channel_closed = true;
        trace_enforcement_state!(self);
        self.persist()?;
        Ok((sig, htlc_sigs))
    }

    /// Sign a holder commitment and HTLCs when recovering from node failure
    /// Also returns the revocable scriptPubKey so we can identify our outputs
    /// Also returns the unilateral close key material
    pub fn sign_holder_commitment_tx_for_recovery(
        &mut self,
    ) -> Result<(Transaction, Vec<Transaction>, Script, (SecretKey, Vec<Vec<u8>>), PublicKey), Status>
    {
        let info2 = self
            .enforcement_state
            .current_holder_commit_info
            .as_ref()
            .expect("holder commitment info should be present");
        let cp_sigs = self
            .enforcement_state
            .current_counterparty_signatures
            .as_ref()
            .expect("counterparty signatures should be present");
        let commitment_number = self.enforcement_state.next_holder_commit_num - 1;
        warn!("force-closing channel for recovery at commitment number {}", commitment_number);

        let htlcs =
            Self::htlcs_info2_to_oic(info2.offered_htlcs.clone(), info2.received_htlcs.clone());
        let per_commitment_point = self.get_per_commitment_point(commitment_number)?;

        let build_feerate = if self.setup.is_zero_fee_htlc() { 0 } else { info2.feerate_per_kw };
        let txkeys = self.make_holder_tx_keys(&per_commitment_point).unwrap();
        // policy-onchain-format-standard
        let recomposed_tx = self.make_holder_commitment_tx(
            commitment_number,
            &txkeys,
            build_feerate,
            info2.to_broadcaster_value_sat,
            info2.to_countersigner_value_sat,
            htlcs,
        );

        // We provide a dummy signature for the remote, since we don't require that sig
        // to be passed in to this call.  It would have been better if HolderCommitmentTransaction
        // didn't require the remote sig.
        // TODO consider if we actually want the sig for policy checks
        let htlcs_len = recomposed_tx.htlcs().len();
        let mut htlc_dummy_sigs = Vec::with_capacity(htlcs_len);
        htlc_dummy_sigs.resize(htlcs_len, Self::dummy_sig());

        // Holder commitments need an extra wrapper for the LDK signature routine.
        let recomposed_holder_tx = HolderCommitmentTransaction::new(
            recomposed_tx,
            Self::dummy_sig(),
            htlc_dummy_sigs,
            &self.keys.pubkeys().funding_pubkey,
            &self.keys.counterparty_pubkeys().funding_pubkey,
        );

        // Sign the recomposed commitment.
        // FIXME handle HTLCs
        let (sig, _htlc_sigs) = self
            .keys
            .sign_holder_commitment_and_htlcs(&recomposed_holder_tx, &self.secp_ctx)
            .map_err(|_| internal_error("failed to sign"))?;

        let holder_tx = recomposed_holder_tx.trust();
        let mut tx = holder_tx.built_transaction().transaction.clone();
        let holder_funding_key = self.keys.pubkeys().funding_pubkey;
        let counterparty_funding_key = self.keys.counterparty_pubkeys().funding_pubkey;

        let tx_keys = holder_tx.keys();
        let revocable_redeemscript = chan_utils::get_revokeable_redeemscript(
            &tx_keys.revocation_key,
            self.setup.counterparty_selected_contest_delay,
            &tx_keys.broadcaster_delayed_payment_key,
        );

        add_holder_sig(&mut tx, sig, cp_sigs.0, &holder_funding_key, &counterparty_funding_key);
        self.enforcement_state.channel_closed = true;
        trace_enforcement_state!(self);

        let revocation_basepoint = self.keys.counterparty_pubkeys().revocation_basepoint;
        let revocation_pubkey = derive_public_revocation_key(
            &self.secp_ctx,
            &per_commitment_point,
            &revocation_basepoint,
        );
        let ck =
            self.get_unilateral_close_key(&Some(per_commitment_point), &Some(revocation_pubkey))?;

        self.persist()?;
        Ok((tx, Vec::new(), revocable_redeemscript.to_v0_p2wsh(), ck, revocation_pubkey))
    }

    /// Sign a holder commitment transaction after rebuilding it
    /// from the supplied arguments.
    /// Use [Channel::sign_counterparty_commitment_tx_phase2()] instead of this,
    /// since that one uses the last counter-signed holder tx, which is simpler
    /// and doesn't require re-validation of the holder tx.
    // TODO anchors support once upstream supports it
    pub fn sign_holder_commitment_tx_phase2_redundant(
        &mut self,
        commitment_number: u64,
        feerate_per_kw: u32,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
    ) -> Result<(Signature, Vec<Signature>), Status> {
        let per_commitment_point = self.get_per_commitment_point(commitment_number)?;

        let info2 = self.build_holder_commitment_info(
            &per_commitment_point,
            to_holder_value_sat,
            to_counterparty_value_sat,
            offered_htlcs.clone(),
            received_htlcs.clone(),
            feerate_per_kw,
        )?;

        self.validator().validate_holder_commitment_tx(
            &self.enforcement_state,
            commitment_number,
            &per_commitment_point,
            &self.setup,
            &self.get_chain_state(),
            &info2,
        )?;

        let htlcs = Self::htlcs_info2_to_oic(offered_htlcs, received_htlcs);

        // We provide a dummy signature for the remote, since we don't require that sig
        // to be passed in to this call.  It would have been better if HolderCommitmentTransaction
        // didn't require the remote sig.
        // TODO consider if we actually want the sig for policy checks
        let mut htlc_dummy_sigs = Vec::with_capacity(htlcs.len());
        htlc_dummy_sigs.resize(htlcs.len(), Self::dummy_sig());

        let build_feerate = if self.setup.is_zero_fee_htlc() { 0 } else { feerate_per_kw };
        let txkeys = self.make_holder_tx_keys(&per_commitment_point).unwrap();
        let commitment_tx = self.make_holder_commitment_tx(
            commitment_number,
            &txkeys,
            build_feerate,
            to_holder_value_sat,
            to_counterparty_value_sat,
            htlcs,
        );
        debug!("channel: sign holder txid {}", commitment_tx.trust().built_transaction().txid);

        let holder_commitment_tx = HolderCommitmentTransaction::new(
            commitment_tx,
            Self::dummy_sig(),
            htlc_dummy_sigs,
            &self.keys.pubkeys().funding_pubkey,
            &self.keys.counterparty_pubkeys().funding_pubkey,
        );

        let (sig, htlc_sigs) = self
            .keys
            .sign_holder_commitment_and_htlcs(&holder_commitment_tx, &self.secp_ctx)
            .map_err(|_| internal_error("failed to sign"))?;

        self.enforcement_state.channel_closed = true;
        trace_enforcement_state!(self);
        self.persist()?;
        Ok((sig, htlc_sigs))
    }

    pub(crate) fn make_holder_commitment_tx(
        &self,
        commitment_number: u64,
        keys: &TxCreationKeys,
        feerate_per_kw: u32,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        htlcs: Vec<HTLCOutputInCommitment>,
    ) -> CommitmentTransaction {
        let mut htlcs_with_aux = htlcs.iter().map(|h| (h.clone(), ())).collect();
        let channel_parameters = self.make_channel_parameters();
        let parameters = channel_parameters.as_holder_broadcastable();
        let mut commitment_tx = CommitmentTransaction::new_with_auxiliary_htlc_data(
            INITIAL_COMMITMENT_NUMBER - commitment_number,
            to_holder_value_sat,
            to_counterparty_value_sat,
            self.setup.is_anchors(),
            self.keys.pubkeys().funding_pubkey,
            self.keys.counterparty_pubkeys().funding_pubkey,
            keys.clone(),
            feerate_per_kw,
            &mut htlcs_with_aux,
            &parameters,
        );
        if self.setup.is_anchors() {
            commitment_tx = commitment_tx.with_non_zero_fee_anchors();
        }
        commitment_tx
    }

    /// Public for testing purposes
    pub fn htlcs_info2_to_oic(
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
    ) -> Vec<HTLCOutputInCommitment> {
        let mut htlcs = Vec::new();
        for htlc in offered_htlcs {
            htlcs.push(HTLCOutputInCommitment {
                offered: true,
                amount_msat: htlc.value_sat * 1000,
                cltv_expiry: htlc.cltv_expiry,
                payment_hash: htlc.payment_hash,
                transaction_output_index: None,
            });
        }
        for htlc in received_htlcs {
            htlcs.push(HTLCOutputInCommitment {
                offered: false,
                amount_msat: htlc.value_sat * 1000,
                cltv_expiry: htlc.cltv_expiry,
                payment_hash: htlc.payment_hash,
                transaction_output_index: None,
            });
        }
        htlcs
    }

    /// Build channel parameters, used to further build a commitment transaction
    pub fn make_channel_parameters(&self) -> ChannelTransactionParameters {
        let funding_outpoint = chain::transaction::OutPoint {
            txid: self.setup.funding_outpoint.txid,
            index: self.setup.funding_outpoint.vout as u16,
        };
        let opt_non_zero_fee_anchors = if self.setup.is_zero_fee_htlc() { Some(()) } else { None };
        let channel_parameters = ChannelTransactionParameters {
            holder_pubkeys: self.get_channel_basepoints(),
            holder_selected_contest_delay: self.setup.holder_selected_contest_delay,
            is_outbound_from_holder: self.setup.is_outbound,
            counterparty_parameters: Some(CounterpartyChannelTransactionParameters {
                pubkeys: self.setup.counterparty_points.clone(),
                selected_contest_delay: self.setup.counterparty_selected_contest_delay,
            }),
            funding_outpoint: Some(funding_outpoint),
            opt_anchors: if self.setup.is_anchors() { Some(()) } else { None },
            opt_non_zero_fee_anchors,
        };
        channel_parameters
    }

    /// Get the shutdown script where our funds will go when we mutual-close
    // FIXME - this method is deprecated
    pub fn get_ldk_shutdown_script(&self) -> Script {
        self.setup
            .holder_shutdown_script
            .clone()
            .unwrap_or_else(|| self.get_node().keys_manager.get_shutdown_scriptpubkey().into())
    }

    fn get_node(&self) -> Arc<Node> {
        self.node.upgrade().unwrap()
    }

    /// Sign a mutual close transaction after rebuilding it from the supplied arguments
    pub fn sign_mutual_close_tx_phase2(
        &mut self,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        holder_script: &Option<Script>,
        counterparty_script: &Option<Script>,
        holder_wallet_path_hint: &[u32],
    ) -> Result<Signature, Status> {
        self.validator().validate_mutual_close_tx(
            &*self.get_node(),
            &self.setup,
            &self.enforcement_state,
            to_holder_value_sat,
            to_counterparty_value_sat,
            holder_script,
            counterparty_script,
            holder_wallet_path_hint,
        )?;

        let tx = ClosingTransaction::new(
            to_holder_value_sat,
            to_counterparty_value_sat,
            holder_script.clone().unwrap_or_else(|| Script::new()),
            counterparty_script.clone().unwrap_or_else(|| Script::new()),
            self.setup.funding_outpoint,
        );

        let sig = self
            .keys
            .sign_closing_transaction(&tx, &self.secp_ctx)
            .map_err(|_| Status::internal("failed to sign"))?;
        self.enforcement_state.channel_closed = true;
        trace_enforcement_state!(self);
        self.persist()?;
        Ok(sig)
    }

    /// Sign a delayed output that goes to us while sweeping a transaction we broadcast
    pub fn sign_delayed_sweep(
        &self,
        tx: &Transaction,
        input: usize,
        commitment_number: u64,
        redeemscript: &Script,
        amount_sat: u64,
        wallet_path: &[u32],
    ) -> Result<Signature, Status> {
        if input >= tx.input.len() {
            return Err(invalid_argument(format!(
                "sign_delayed_sweep: bad input index: {} >= {}",
                input,
                tx.input.len()
            )));
        }
        let per_commitment_point = self.get_per_commitment_point(commitment_number)?;

        self.validator().validate_delayed_sweep(
            &*self.get_node(),
            &self.setup,
            &self.get_chain_state(),
            tx,
            input,
            amount_sat,
            wallet_path,
        )?;

        let sighash = Message::from_slice(
            &SighashCache::new(tx)
                .segwit_signature_hash(input, &redeemscript, amount_sat, EcdsaSighashType::All)
                .unwrap()[..],
        )
        .map_err(|_| Status::internal("failed to sighash"))?;

        let privkey = derive_private_key(
            &self.secp_ctx,
            &per_commitment_point,
            &self.keys.delayed_payment_base_key,
        );

        let sig = self.secp_ctx.sign_ecdsa(&sighash, &privkey);
        trace_enforcement_state!(self);
        self.persist()?;
        Ok(sig)
    }

    /// Sign an offered or received HTLC output from a commitment the counterparty broadcast.
    pub fn sign_counterparty_htlc_sweep(
        &self,
        tx: &Transaction,
        input: usize,
        remote_per_commitment_point: &PublicKey,
        redeemscript: &Script,
        htlc_amount_sat: u64,
        wallet_path: &[u32],
    ) -> Result<Signature, Status> {
        if input >= tx.input.len() {
            return Err(invalid_argument(format!(
                "sign_counterparty_htlc_sweep: bad input index: {} >= {}",
                input,
                tx.input.len()
            )));
        }

        self.validator().validate_counterparty_htlc_sweep(
            &*self.get_node(),
            &self.setup,
            &self.get_chain_state(),
            tx,
            redeemscript,
            input,
            htlc_amount_sat,
            wallet_path,
        )?;

        let htlc_sighash = Message::from_slice(
            &SighashCache::new(tx)
                .segwit_signature_hash(input, &redeemscript, htlc_amount_sat, EcdsaSighashType::All)
                .unwrap()[..],
        )
        .map_err(|_| Status::internal("failed to sighash"))?;

        let htlc_privkey = derive_private_key(
            &self.secp_ctx,
            &remote_per_commitment_point,
            &self.keys.htlc_base_key,
        );

        let sig = self.secp_ctx.sign_ecdsa(&htlc_sighash, &htlc_privkey);
        trace_enforcement_state!(self);
        self.persist()?;
        Ok(sig)
    }

    /// Sign a justice transaction on an old state that the counterparty broadcast
    pub fn sign_justice_sweep(
        &self,
        tx: &Transaction,
        input: usize,
        revocation_secret: &SecretKey,
        redeemscript: &Script,
        amount_sat: u64,
        wallet_path: &[u32],
    ) -> Result<Signature, Status> {
        if input >= tx.input.len() {
            return Err(invalid_argument(format!(
                "sign_justice_sweep: bad input index: {} >= {}",
                input,
                tx.input.len()
            )));
        }
        self.validator().validate_justice_sweep(
            &*self.get_node(),
            &self.setup,
            &self.get_chain_state(),
            tx,
            input,
            amount_sat,
            wallet_path,
        )?;

        let sighash = Message::from_slice(
            &SighashCache::new(tx)
                .segwit_signature_hash(input, &redeemscript, amount_sat, EcdsaSighashType::All)
                .unwrap()[..],
        )
        .map_err(|_| Status::internal("failed to sighash"))?;

        let privkey = chan_utils::derive_private_revocation_key(
            &self.secp_ctx,
            revocation_secret,
            &self.keys.revocation_base_key,
        );

        let sig = self.secp_ctx.sign_ecdsa(&sighash, &privkey);
        trace_enforcement_state!(self);
        self.persist()?;
        Ok(sig)
    }

    /// Sign a channel announcement with both the node key and the funding key
    pub fn sign_channel_announcement_with_funding_key(&self, announcement: &[u8]) -> Signature {
        let ann_hash = Sha256dHash::hash(announcement);
        let encmsg = secp256k1::Message::from_slice(&ann_hash[..]).expect("encmsg failed");

        self.secp_ctx.sign_ecdsa(&encmsg, &self.keys.funding_key)
    }

    fn persist(&self) -> Result<(), Status> {
        let node_id = self.get_node().get_id();
        self.get_node()
            .persister
            .update_channel(&node_id, &self)
            .map_err(|_| Status::internal("persist failed"))
    }

    /// The node's network
    pub fn network(&self) -> Network {
        self.get_node().network()
    }

    /// The node has signed our funding transaction
    pub fn funding_signed(&self, tx: &Transaction, _vout: u32) {
        // Start tracking funding inputs, in case an input is double-spent.
        // The tracker was already informed of the inputs by the node.

        // Note that the fundee won't detect double-spends this way, because
        // they never see the details of the funding transaction.
        // But the fundee doesn't care about double-spends, because they don't
        // have any funds in the channel yet.

        // the lock order is backwards (monitor -> tracker), but we release
        // the monitor lock, so it's OK

        self.monitor.add_funding_inputs(tx);
    }

    /// Return channel balances
    pub fn balance(&self) -> ChannelBalance {
        let node = self.get_node();
        let state = node.get_state();
        self.enforcement_state.balance(&*state, &self.setup)
    }

    /// advance the holder commitment in an arbitrary way for testing
    #[cfg(feature = "test_utils")]
    pub fn advance_holder_commitment(
        &mut self,
        counterparty_key: &SecretKey,
        counterparty_htlc_key: &SecretKey,
        offered_htlcs: Vec<HTLCInfo2>,
        value_to_holder: u64,
        commit_num: u64,
    ) -> Result<(), Status> {
        let feerate = 1000;
        let funding_redeemscript = make_funding_redeemscript(
            &self.keys.pubkeys().funding_pubkey,
            &self.keys.counterparty_pubkeys().funding_pubkey,
        );
        let per_commitment_point = self.get_per_commitment_point(commit_num)?;
        let txkeys = self.make_holder_tx_keys(&per_commitment_point).unwrap();

        let tx = self.make_holder_commitment_tx(
            commit_num,
            &txkeys,
            feerate,
            value_to_holder,
            0,
            Channel::htlcs_info2_to_oic(offered_htlcs.clone(), vec![]),
        );

        let trusted_tx = tx.trust();
        let built_tx = trusted_tx.built_transaction();
        let counterparty_sig = built_tx.sign_counterparty_commitment(
            &counterparty_key,
            &funding_redeemscript,
            self.setup.channel_value_sat,
            &self.secp_ctx,
        );

        let counterparty_htlc_key =
            derive_private_key(&self.secp_ctx, &per_commitment_point, &counterparty_htlc_key);

        let mut htlc_sigs = Vec::with_capacity(tx.htlcs().len());
        for htlc in tx.htlcs() {
            let htlc_tx = build_htlc_transaction(
                &trusted_tx.txid(),
                feerate,
                self.setup.counterparty_selected_contest_delay,
                htlc,
                self.setup.is_anchors(),
                !self.setup.is_zero_fee_htlc(),
                &txkeys.broadcaster_delayed_payment_key,
                &txkeys.revocation_key,
            );
            let htlc_redeemscript = get_htlc_redeemscript(&htlc, self.setup.is_anchors(), &txkeys);
            let sig_hash_type = if self.setup.is_anchors() {
                EcdsaSighashType::SinglePlusAnyoneCanPay
            } else {
                EcdsaSighashType::All
            };
            let htlc_sighash = Message::from_slice(
                &SighashCache::new(&htlc_tx)
                    .segwit_signature_hash(
                        0,
                        &htlc_redeemscript,
                        htlc.amount_msat / 1000,
                        sig_hash_type,
                    )
                    .unwrap()[..],
            )
            .unwrap();
            htlc_sigs.push(self.secp_ctx.sign_ecdsa(&htlc_sighash, &counterparty_htlc_key));
        }

        // add an HTLC
        self.validate_holder_commitment_tx_phase2(
            commit_num,
            feerate,
            value_to_holder,
            0,
            offered_htlcs,
            vec![],
            &counterparty_sig,
            &htlc_sigs,
        )?;
        Ok(())
    }
}

/// Balances associated with a channel
/// See: <https://gitlab.com/lightning-signer/docs/-/wikis/Proposed-L1-and-Channel-Balance-Reconciliation>
#[derive(Debug, PartialEq)]
pub struct ChannelBalance {
    /// Claimable balance on open channel
    pub claimable: u64,
    /// Sum of htlcs offered to us
    pub received_htlc: u64,
    /// Sum of htlcs we offered
    pub offered_htlc: u64,
    /// Sweeping to wallet
    pub sweeping: u64,
    /// Current number of channels
    pub channel_count: u32,
    /// Current number of received htlcs
    pub received_htlc_count: u32,
    /// Current number of offered htlcs
    pub offered_htlc_count: u32,
}

impl ChannelBalance {
    /// Create a ChannelBalance with specific values
    pub fn new(
        claimable: u64,
        received_htlc: u64,
        offered_htlc: u64,
        sweeping: u64,
        channel_count: u32,
        received_htlc_count: u32,
        offered_htlc_count: u32,
    ) -> ChannelBalance {
        ChannelBalance {
            claimable,
            received_htlc,
            offered_htlc,
            sweeping,
            channel_count,
            received_htlc_count,
            offered_htlc_count,
        }
    }

    /// Create a ChannelBalance with zero values
    pub fn zero() -> ChannelBalance {
        ChannelBalance {
            claimable: 0,
            received_htlc: 0,
            offered_htlc: 0,
            sweeping: 0,
            channel_count: 0,
            received_htlc_count: 0,
            offered_htlc_count: 0,
        }
    }

    /// Sum channel balances
    pub fn accumulate(&mut self, other: &ChannelBalance) {
        self.claimable += other.claimable;
        self.received_htlc += other.received_htlc;
        self.offered_htlc += other.offered_htlc;
        self.sweeping += other.sweeping;
        self.channel_count += other.channel_count;
        self.received_htlc_count += other.received_htlc_count;
        self.offered_htlc_count += other.offered_htlc_count;
    }
}

// Phase 1
impl Channel {
    pub(crate) fn build_counterparty_commitment_info(
        &self,
        remote_per_commitment_point: &PublicKey,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
        feerate_per_kw: u32,
    ) -> Result<CommitmentInfo2, Status> {
        let holder_points = self.keys.pubkeys();
        let secp_ctx = &self.secp_ctx;

        let to_counterparty_delayed_pubkey = derive_public_key(
            secp_ctx,
            &remote_per_commitment_point,
            &self.setup.counterparty_points.delayed_payment_basepoint,
        )
        .map_err(|err| {
            internal_error(format!("could not derive to_holder_delayed_key: {}", err))
        })?;
        let counterparty_payment_pubkey =
            self.derive_counterparty_payment_pubkey(remote_per_commitment_point)?;
        let revocation_pubkey = chan_utils::derive_public_revocation_key(
            secp_ctx,
            &remote_per_commitment_point,
            &holder_points.revocation_basepoint,
        );
        let to_holder_pubkey = counterparty_payment_pubkey.clone();
        Ok(CommitmentInfo2::new(
            true,
            to_holder_pubkey,
            to_holder_value_sat,
            revocation_pubkey,
            to_counterparty_delayed_pubkey,
            to_counterparty_value_sat,
            self.setup.holder_selected_contest_delay,
            offered_htlcs,
            received_htlcs,
            feerate_per_kw,
        ))
    }

    fn build_holder_commitment_info(
        &self,
        per_commitment_point: &PublicKey,
        to_holder_value_sat: u64,
        to_counterparty_value_sat: u64,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
        feerate_per_kw: u32,
    ) -> Result<CommitmentInfo2, Status> {
        let holder_points = self.keys.pubkeys();
        let counterparty_points = self.keys.counterparty_pubkeys();
        let secp_ctx = &self.secp_ctx;

        let to_holder_delayed_pubkey = derive_public_key(
            secp_ctx,
            &per_commitment_point,
            &holder_points.delayed_payment_basepoint,
        )
        .map_err(|err| {
            internal_error(format!("could not derive to_holder_delayed_pubkey: {}", err))
        })?;

        let counterparty_pubkey = if self.setup.is_static_remotekey() {
            counterparty_points.payment_point
        } else {
            derive_public_key(
                &self.secp_ctx,
                &per_commitment_point,
                &counterparty_points.payment_point,
            )
            .map_err(|err| {
                internal_error(format!("could not derive counterparty_pubkey: {}", err))
            })?
        };

        let revocation_pubkey = chan_utils::derive_public_revocation_key(
            secp_ctx,
            &per_commitment_point,
            &counterparty_points.revocation_basepoint,
        );
        let to_counterparty_pubkey = counterparty_pubkey.clone();
        Ok(CommitmentInfo2::new(
            false,
            to_counterparty_pubkey,
            to_counterparty_value_sat,
            revocation_pubkey,
            to_holder_delayed_pubkey,
            to_holder_value_sat,
            self.setup.counterparty_selected_contest_delay,
            offered_htlcs,
            received_htlcs,
            feerate_per_kw,
        ))
    }

    /// Phase 1
    pub fn sign_counterparty_commitment_tx(
        &mut self,
        tx: &Transaction,
        output_witscripts: &[Vec<u8>],
        remote_per_commitment_point: &PublicKey,
        commitment_number: u64,
        feerate_per_kw: u32,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
    ) -> Result<Signature, Status> {
        if tx.output.len() != output_witscripts.len() {
            return Err(invalid_argument("len(tx.output) != len(witscripts)"));
        }

        // Since we didn't have the value at the real open, validate it now.
        let validator = self.validator();
        validator.validate_channel_value(&self.setup)?;

        // Derive a CommitmentInfo first, convert to CommitmentInfo2 below ...
        let is_counterparty = true;
        let info = validator.decode_commitment_tx(
            &self.keys,
            &self.setup,
            is_counterparty,
            tx,
            output_witscripts,
        )?;

        let info2 = self.build_counterparty_commitment_info(
            remote_per_commitment_point,
            info.to_countersigner_value_sat,
            info.to_broadcaster_value_sat,
            offered_htlcs,
            received_htlcs,
            feerate_per_kw,
        )?;

        let node = self.get_node();
        let mut state = node.get_state();
        let delta =
            self.enforcement_state.claimable_balances(&*state, None, Some(&info2), &self.setup);

        let incoming_payment_summary =
            self.enforcement_state.incoming_payments_summary(None, Some(&info2));

        validator
            .validate_counterparty_commitment_tx(
                &self.enforcement_state,
                commitment_number,
                &remote_per_commitment_point,
                &self.setup,
                &self.get_chain_state(),
                &info2,
            )
            .map_err(|ve| {
                debug!(
                    "VALIDATION FAILED: {}\ntx={:#?}\nsetup={:#?}\ncstate={:#?}\ninfo={:#?}",
                    ve,
                    &tx,
                    &self.setup,
                    &self.get_chain_state(),
                    &info2,
                );
                ve
            })?;

        let htlcs =
            Self::htlcs_info2_to_oic(info2.offered_htlcs.clone(), info2.received_htlcs.clone());

        let recomposed_tx = self.make_counterparty_commitment_tx(
            remote_per_commitment_point,
            commitment_number,
            feerate_per_kw,
            info.to_countersigner_value_sat,
            info.to_broadcaster_value_sat,
            htlcs,
        );

        if recomposed_tx.trust().built_transaction().transaction != *tx {
            debug!("ORIGINAL_TX={:#?}", &tx);
            debug!("RECOMPOSED_TX={:#?}", &recomposed_tx.trust().built_transaction().transaction);
            policy_err!(validator, "policy-commitment", "recomposed tx mismatch");
        }

        // The comparison in the previous block will fail if any of the
        // following policies are violated:
        // - policy-commitment-version
        // - policy-commitment-locktime
        // - policy-commitment-sequence
        // - policy-commitment-input-single
        // - policy-commitment-input-match-funding
        // - policy-commitment-revocation-pubkey
        // - policy-commitment-broadcaster-pubkey
        // - policy-commitment-countersignatory-pubkey
        // - policy-commitment-htlc-revocation-pubkey
        // - policy-commitment-htlc-counterparty-htlc-pubkey
        // - policy-commitment-htlc-holder-htlc-pubkey
        // - policy-commitment-singular
        // - policy-commitment-to-self-delay
        // - policy-commitment-no-unrecognized-outputs

        // Convert from backwards counting.
        let commit_num = INITIAL_COMMITMENT_NUMBER - recomposed_tx.trust().commitment_number();

        let point = recomposed_tx.trust().keys().per_commitment_point;

        // Sign the recomposed commitment.
        let sigs = self
            .keys
            .sign_counterparty_commitment(&recomposed_tx, Vec::new(), &self.secp_ctx)
            .map_err(|_| internal_error(format!("sign_counterparty_commitment failed")))?;

        let outgoing_payment_summary = self.enforcement_state.payments_summary(None, Some(&info2));
        state.validate_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator.clone(),
        )?;

        // Only advance the state if nothing goes wrong.
        validator.set_next_counterparty_commit_num(
            &mut self.enforcement_state,
            commit_num + 1,
            point,
            info2,
        )?;

        state.apply_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator,
        );

        trace_enforcement_state!(self);
        self.persist()?;

        // Discard the htlc signatures for now.
        Ok(sigs.0)
    }

    fn make_validated_recomposed_holder_commitment_tx(
        &self,
        tx: &Transaction,
        output_witscripts: &[Vec<u8>],
        commitment_number: u64,
        per_commitment_point: PublicKey,
        txkeys: &TxCreationKeys,
        feerate_per_kw: u32,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
    ) -> Result<(CommitmentTransaction, CommitmentInfo2, Map<PaymentHash, u64>), Status> {
        if tx.output.len() != output_witscripts.len() {
            return Err(invalid_argument(format!(
                "len(tx.output):{} != len(witscripts):{}",
                tx.output.len(),
                output_witscripts.len()
            )));
        }

        let validator = self.validator();

        // Since we didn't have the value at the real open, validate it now.
        validator.validate_channel_value(&self.setup)?;

        // Derive a CommitmentInfo first, convert to CommitmentInfo2 below ...
        let is_counterparty = false;
        let info = validator.decode_commitment_tx(
            &self.keys,
            &self.setup,
            is_counterparty,
            tx,
            output_witscripts,
        )?;

        let info2 = self.build_holder_commitment_info(
            &per_commitment_point,
            info.to_broadcaster_value_sat,
            info.to_countersigner_value_sat,
            offered_htlcs.clone(),
            received_htlcs.clone(),
            feerate_per_kw,
        )?;

        let incoming_payment_summary =
            self.enforcement_state.incoming_payments_summary(Some(&info2), None);

        validator
            .validate_holder_commitment_tx(
                &self.enforcement_state,
                commitment_number,
                &per_commitment_point,
                &self.setup,
                &self.get_chain_state(),
                &info2,
            )
            .map_err(|ve| {
                warn!(
                    "VALIDATION FAILED: {}\ntx={:#?}\nsetup={:#?}\nstate={:#?}\ninfo={:#?}",
                    ve,
                    &tx,
                    &self.setup,
                    &self.get_chain_state(),
                    &info2,
                );
                ve
            })?;

        let htlcs =
            Self::htlcs_info2_to_oic(info2.offered_htlcs.clone(), info2.received_htlcs.clone());

        let recomposed_tx = self.make_holder_commitment_tx(
            commitment_number,
            txkeys,
            feerate_per_kw,
            info.to_broadcaster_value_sat,
            info.to_countersigner_value_sat,
            htlcs.clone(),
        );

        if recomposed_tx.trust().built_transaction().transaction != *tx {
            dbgvals!(
                &self.setup,
                &self.enforcement_state,
                tx,
                DebugVecVecU8(output_witscripts),
                commitment_number,
                feerate_per_kw,
                &offered_htlcs,
                &received_htlcs
            );
            warn!("RECOMPOSITION FAILED");
            warn!("ORIGINAL_TX={:#?}", &tx);
            warn!("RECOMPOSED_TX={:#?}", &recomposed_tx.trust().built_transaction().transaction);
            policy_err!(validator, "policy-commitment", "recomposed tx mismatch");
        }

        // The comparison in the previous block will fail if any of the
        // following policies are violated:
        // - policy-commitment-version
        // - policy-commitment-locktime
        // - policy-commitment-sequence
        // - policy-commitment-input-single
        // - policy-commitment-input-match-funding
        // - policy-commitment-revocation-pubkey
        // - policy-commitment-broadcaster-pubkey
        // - policy-commitment-htlc-revocation-pubkey
        // - policy-commitment-htlc-counterparty-htlc-pubkey
        // - policy-commitment-htlc-holder-htlc-pubkey
        // - policy-revoke-new-commitment-valid

        Ok((recomposed_tx, info2, incoming_payment_summary))
    }

    /// Validate the counterparty's signatures on the holder's
    /// commitment and HTLCs when the commitment_signed message is
    /// received.  Returns the next per_commitment_point and the
    /// holder's revocation secret for the prior commitment.  This
    /// method advances the expected next holder commitment number in
    /// the signer's state.
    pub fn validate_holder_commitment_tx(
        &mut self,
        tx: &Transaction,
        output_witscripts: &[Vec<u8>],
        commitment_number: u64,
        feerate_per_kw: u32,
        offered_htlcs: Vec<HTLCInfo2>,
        received_htlcs: Vec<HTLCInfo2>,
        counterparty_commit_sig: &Signature,
        counterparty_htlc_sigs: &[Signature],
    ) -> Result<(PublicKey, Option<SecretKey>), Status> {
        let validator = self.validator();
        let per_commitment_point = self.get_per_commitment_point(commitment_number)?;
        let txkeys = self
            .make_holder_tx_keys(&per_commitment_point)
            .map_err(|err| internal_error(format!("make_holder_tx_keys failed: {}", err)))?;

        // policy-onchain-format-standard
        let (recomposed_tx, info2, incoming_payment_summary) = self
            .make_validated_recomposed_holder_commitment_tx(
                tx,
                output_witscripts,
                commitment_number,
                per_commitment_point,
                &txkeys,
                feerate_per_kw,
                offered_htlcs,
                received_htlcs,
            )?;

        let node = self.get_node();
        let mut state = node.get_state();
        let delta =
            self.enforcement_state.claimable_balances(&*state, Some(&info2), None, &self.setup);

        self.check_holder_tx_signatures(
            &per_commitment_point,
            &txkeys,
            feerate_per_kw,
            counterparty_commit_sig,
            counterparty_htlc_sigs,
            recomposed_tx,
        )?;

        let outgoing_payment_summary = self.enforcement_state.payments_summary(Some(&info2), None);
        state.validate_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator.clone(),
        )?;

        let counterparty_signatures =
            CommitmentSignatures(counterparty_commit_sig.clone(), counterparty_htlc_sigs.to_vec());
        let (next_holder_commitment_point, maybe_old_secret) = self
            .advance_holder_commitment_state(
                validator.clone(),
                commitment_number,
                info2,
                counterparty_signatures,
            )?;

        state.apply_payments(
            &self.id0,
            &incoming_payment_summary,
            &outgoing_payment_summary,
            &delta,
            validator,
        );

        trace_enforcement_state!(self);
        self.persist()?;

        Ok((next_holder_commitment_point, maybe_old_secret))
    }

    /// Process the counterparty's revocation
    ///
    /// When this is provided, we know that the counterparty has committed to
    /// the next state.
    pub fn validate_counterparty_revocation(
        &mut self,
        revoke_num: u64,
        old_secret: &SecretKey,
    ) -> Result<(), Status> {
        // TODO - need to store the revealed secret.

        let validator = self.validator();
        validator.validate_counterparty_revocation(
            &self.enforcement_state,
            revoke_num,
            old_secret,
        )?;

        if let Some(secrets) = self.enforcement_state.counterparty_secrets.as_mut() {
            let backwards_num = INITIAL_COMMITMENT_NUMBER - revoke_num;
            if secrets.provide_secret(backwards_num, old_secret.secret_bytes()).is_err() {
                error!(
                    "secret does not chain: {} ({}) {} into {:?}",
                    revoke_num,
                    backwards_num,
                    old_secret.display_secret(),
                    secrets
                );
                policy_err!(
                    validator,
                    "policy-commitment-previous-revoked",
                    "counterparty secret does not chain"
                )
            }
        }

        validator.set_next_counterparty_revoke_num(&mut self.enforcement_state, revoke_num + 1)?;

        trace_enforcement_state!(self);
        self.persist()?;
        Ok(())
    }

    /// Phase 1
    pub fn sign_mutual_close_tx(
        &mut self,
        tx: &Transaction,
        opaths: &[Vec<u32>],
    ) -> Result<Signature, Status> {
        dbgvals!(tx.txid(), self.get_node().allowlist().unwrap());
        if opaths.len() != tx.output.len() {
            return Err(invalid_argument(format!(
                "{}: bad opath len {} with tx.output len {}",
                short_function!(),
                opaths.len(),
                tx.output.len()
            )));
        }

        let recomposed_tx = self.validator().decode_and_validate_mutual_close_tx(
            &*self.get_node(),
            &self.setup,
            &self.enforcement_state,
            tx,
            opaths,
        )?;

        let sig = self
            .keys
            .sign_closing_transaction(&recomposed_tx, &self.secp_ctx)
            .map_err(|_| Status::internal("failed to sign"))?;
        self.enforcement_state.channel_closed = true;
        trace_enforcement_state!(self);
        self.persist()?;
        Ok(sig)
    }

    /// Phase 1
    pub fn sign_holder_htlc_tx(
        &self,
        tx: &Transaction,
        commitment_number: u64,
        opt_per_commitment_point: Option<PublicKey>,
        redeemscript: &Script,
        htlc_amount_sat: u64,
        output_witscript: &Script,
    ) -> Result<TypedSignature, Status> {
        let per_commitment_point = if opt_per_commitment_point.is_some() {
            opt_per_commitment_point.unwrap()
        } else {
            self.get_per_commitment_point(commitment_number)?
        };

        let txkeys =
            self.make_holder_tx_keys(&per_commitment_point).expect("failed to make txkeys");

        self.sign_htlc_tx(
            tx,
            &per_commitment_point,
            redeemscript,
            htlc_amount_sat,
            output_witscript,
            false, // is_counterparty
            txkeys,
        )
    }

    /// Phase 1
    pub fn sign_counterparty_htlc_tx(
        &self,
        tx: &Transaction,
        remote_per_commitment_point: &PublicKey,
        redeemscript: &Script,
        htlc_amount_sat: u64,
        output_witscript: &Script,
    ) -> Result<TypedSignature, Status> {
        let txkeys = self
            .make_counterparty_tx_keys(&remote_per_commitment_point)
            .expect("failed to make txkeys");

        self.sign_htlc_tx(
            tx,
            remote_per_commitment_point,
            redeemscript,
            htlc_amount_sat,
            output_witscript,
            true, // is_counterparty
            txkeys,
        )
    }

    /// Sign a 2nd level HTLC transaction hanging off a commitment transaction
    pub fn sign_htlc_tx(
        &self,
        tx: &Transaction,
        per_commitment_point: &PublicKey,
        redeemscript: &Script,
        htlc_amount_sat: u64,
        output_witscript: &Script,
        is_counterparty: bool,
        txkeys: TxCreationKeys,
    ) -> Result<TypedSignature, Status> {
        let (feerate_per_kw, htlc, recomposed_tx_sighash, sighash_type) =
            self.validator().decode_and_validate_htlc_tx(
                is_counterparty,
                &self.setup,
                &txkeys,
                tx,
                &redeemscript,
                htlc_amount_sat,
                output_witscript,
            )?;

        self.validator()
            .validate_htlc_tx(
                &self.setup,
                &self.get_chain_state(),
                is_counterparty,
                &htlc,
                feerate_per_kw,
            )
            .map_err(|ve| {
                debug!(
                    "VALIDATION FAILED: {}\n\
                     setup={:#?}\n\
                     state={:#?}\n\
                     is_counterparty={}\n\
                     tx={:#?}\n\
                     htlc={:#?}\n\
                     feerate_per_kw={}",
                    ve,
                    &self.setup,
                    &self.get_chain_state(),
                    is_counterparty,
                    &tx,
                    DebugHTLCOutputInCommitment(&htlc),
                    feerate_per_kw,
                );
                ve
            })?;

        let htlc_privkey =
            derive_private_key(&self.secp_ctx, &per_commitment_point, &self.keys.htlc_base_key);

        let htlc_sighash = Message::from_slice(&recomposed_tx_sighash[..])
            .map_err(|_| Status::internal("failed to sighash recomposed"))?;

        Ok(TypedSignature {
            sig: self.secp_ctx.sign_ecdsa(&htlc_sighash, &htlc_privkey),
            typ: sighash_type,
        })
    }

    /// Get the unilateral close key and the witness stack suffix,
    /// for sweeping the to-remote output of a counterparty's force-close
    // TODO(devrandom) key leaking from this layer
    pub fn get_unilateral_close_key(
        &self,
        commitment_point_opt: &Option<PublicKey>,
        revocation_pubkey: &Option<PublicKey>,
    ) -> Result<(SecretKey, Vec<Vec<u8>>), Status> {
        Ok(match commitment_point_opt {
            Some(commitment_point) => {
                let base_key = if revocation_pubkey.is_some() {
                    &self.keys.delayed_payment_base_key
                } else {
                    &self.keys.payment_key
                };
                let key = derive_private_key(&self.secp_ctx, &commitment_point, base_key);
                let pubkey = PublicKey::from_secret_key(&self.secp_ctx, &key);

                let witness_stack_prefix = if let Some(r) = revocation_pubkey {
                    let contest_delay = self.setup.counterparty_selected_contest_delay;
                    let redeemscript =
                        chan_utils::get_revokeable_redeemscript(r, contest_delay, &pubkey)
                            .to_bytes();
                    vec![vec![], redeemscript]
                } else {
                    vec![PublicKey::from_secret_key(&self.secp_ctx, &base_key).serialize().to_vec()]
                };
                (key, witness_stack_prefix)
            }
            None => {
                if revocation_pubkey.is_some() {
                    return Err(invalid_argument("delayed output without commitment point"));
                }
                // option_static_remotekey in effect
                let key = self.keys.payment_key.clone();
                let redeemscript =
                    PublicKey::from_secret_key(&self.secp_ctx, &key).serialize().to_vec();
                (key, vec![redeemscript])
            }
        })
    }

    /// Mark any in-flight payments (outgoing HTLCs) on this channel with the
    /// given preimage as filled.
    /// Any such payments adjust our expected balance downwards.
    pub fn htlcs_fulfilled(&mut self, preimages: Vec<PaymentPreimage>) {
        let validator = self.validator();
        let node = self.get_node();
        node.htlcs_fulfilled(&self.id0, preimages, validator);
    }

    fn dummy_sig() -> Signature {
        Signature::from_compact(&Vec::from_hex("eb299947b140c0e902243ee839ca58c71291f4cce49ac0367fb4617c4b6e890f18bc08b9be6726c090af4c6b49b2277e134b34078f710a72a5752e39f0139149").unwrap()).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use crate::channel::ChannelBase;
    use crate::util::test_utils::{
        init_node_and_channel, make_test_channel_setup, TEST_NODE_CONFIG, TEST_SEED,
    };
    use bitcoin::hashes::hex::ToHex;
    use bitcoin::psbt::serialize::Serialize;
    use bitcoin::secp256k1::{self, Secp256k1, SecretKey};
    use lightning::ln::chan_utils::HTLCOutputInCommitment;
    use lightning::ln::PaymentHash;

    #[test]
    fn test_dummy_sig() {
        let dummy_sig = Secp256k1::new().sign_ecdsa(
            &secp256k1::Message::from_slice(&[42; 32]).unwrap(),
            &SecretKey::from_slice(&[42; 32]).unwrap(),
        );
        let ser = dummy_sig.serialize_compact();
        assert_eq!("eb299947b140c0e902243ee839ca58c71291f4cce49ac0367fb4617c4b6e890f18bc08b9be6726c090af4c6b49b2277e134b34078f710a72a5752e39f0139149", ser.to_hex());
    }

    #[test]
    fn tx_size_test() {
        let (node, channel_id) =
            init_node_and_channel(TEST_NODE_CONFIG, TEST_SEED[1], make_test_channel_setup());
        node.with_ready_channel(&channel_id, |chan| {
            let n = 1;
            let commitment_point = chan.get_per_commitment_point(n).unwrap();
            let txkeys = chan.make_holder_tx_keys(&commitment_point).unwrap();
            let htlcs = (0..583)
                .map(|i| HTLCOutputInCommitment {
                    offered: true,
                    amount_msat: 1000000,
                    cltv_expiry: 100,
                    payment_hash: PaymentHash([0; 32]),
                    transaction_output_index: Some(i),
                })
                .collect();
            let tx = chan.make_holder_commitment_tx(n, &txkeys, 1, 1, 1, htlcs);
            let tx_size = tx.trust().built_transaction().transaction.serialize().len();
            assert_eq!(tx_size, 25196);
            Ok(())
        })
        .unwrap();
    }
}
