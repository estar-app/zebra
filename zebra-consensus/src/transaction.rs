//! Asynchronous verification of transactions.

use std::{
    collections::HashMap,
    future::Future,
    iter::FromIterator,
    pin::Pin,
    sync::{Arc, Mutex},
    task::{Context, Poll},
};

use chrono::{DateTime, Utc, Duration};
use futures::{
    stream::{FuturesUnordered, StreamExt},
    FutureExt,
};

use tower::{timeout::Timeout, Service, ServiceExt};
use tracing::Instrument;

use zebra_chain::{
    amount::{Amount, NonNegative, Error as AmountError, NegativeAllowed},
    block::{self, Block}, orchard,
    parameters::{Network, NetworkUpgrade},
    primitives::{Groth16Proof},
    sapling,
    transaction::{
        self, HashType, SigHash, Transaction, UnminedTx, UnminedTxId, VerifiedUnminedTx, LockTime,
    },
    transparent::{self, OrderedUtxo}, komodo_hardfork::NN, interest::KOMODO_MAXMEMPOOLTIME, work::difficulty::{CompactDifficulty},
    serialization::ZcashSerialize,
};

use zebra_script::CachedFfiTransaction;
use zebra_state as zs;
use zs::HashOrHeight;

use crate::{error::TransactionError, groth16::DescriptionWrapper, primitives, script, BoxError};

pub mod check;
mod komodo_fee_check;

use komodo_fee_check::{FeeRate, DEFAULT_MIN_RELAY_TX_FEE};

use self::komodo_fee_check::{FeeRateLimiter};

#[cfg(test)]
mod tests;

/// A timeout applied to UTXO lookup requests.
///
/// The exact value is non-essential, but this should be long enough to allow
/// out-of-order verification of blocks (UTXOs are not required to be ready
/// immediately) while being short enough to:
///   * prune blocks that are too far in the future to be worth keeping in the
///     queue,
///   * fail blocks that reference invalid UTXOs, and
///   * fail blocks that reference UTXOs from blocks that have temporarily failed
///     to download, because a peer sent Zebra a bad list of block hashes. (The
///     UTXO verification failure will restart the sync, and re-download the
///     chain in the correct order.)
const UTXO_LOOKUP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6 * 60);

/// Asynchronous transaction verification.
///
/// # Correctness
///
/// Transaction verification requests should be wrapped in a timeout, so that
/// out-of-order and invalid requests do not hang indefinitely. See the [`chain`](`crate::chain`)
/// module documentation for details.
#[derive(Debug, Clone)]
pub struct Verifier<ZS> {
    network: Network,
    state: Timeout<ZS>,
    script_verifier: script::Verifier,

    /// komodo tx min relay tx fee calc 
    min_relay_txfee: FeeRate,

    /// komodo tx with low fee rate limiter
    rate_limiter: Arc<Mutex<FeeRateLimiter>>,
}

impl<ZS> Verifier<ZS>
where
    ZS: Service<zs::Request, Response = zs::Response, Error = BoxError> + Send + Clone + 'static,
    ZS::Future: Send + 'static,
{
    /// Create a new transaction verifier.
    pub fn new(network: Network, state: ZS) -> Self {
        Self {
            network,
            state: Timeout::new(state, UTXO_LOOKUP_TIMEOUT),
            script_verifier: script::Verifier::default(),
            min_relay_txfee: FeeRate::new(Amount::try_from(DEFAULT_MIN_RELAY_TX_FEE).expect("valid min fee default")),
            rate_limiter: Arc::new(Mutex::new(FeeRateLimiter::new())),
        }
    }

    /// create request to await for the last block and return its time
    fn get_last_block_time(state: &Timeout<ZS>, req: &Request) -> impl Future<Output = Result<DateTime<Utc>, TransactionError>>  {
        let state = state.clone();
    
        let req = req.clone();
        async move {
            match req {
                Request::Block { previous_hash, .. } => {
                    let query = state.oneshot(
                        zebra_state::Request::AwaitBlock(
                            previous_hash
                        ));
    
                    match query.await?  {
                        zebra_state::Response::Block(Some(last_block)) => {
                            Ok(last_block.header.time)
                        },
                        zebra_state::Response::Block(None) => { tracing::info!("cannot await for previous block {:?}", previous_hash);  Err(TransactionError::KomodoTipTimeError) },
                        _ => unreachable!("Incorrect response from state service"),
                    }    
                },
                Request::Mempool { .. } => {
                    let query = state.oneshot(
                        zebra_state::Request::Block(HashOrHeight::Height(
                            (req.height() - 1).expect("current block height should be always valid")
                        )));
                    match query.await?    {
                        zebra_state::Response::Block(Some(last_block)) => {
                            Ok(last_block.header.time)
                        },
                        zebra_state::Response::Block(None) => { tracing::info!("cannot get block {:?}", req.height() - 1);  Err(TransactionError::KomodoTipTimeError) }, 
                        _ => unreachable!("Incorrect response from state service"),
                    }
                },
            }
        }
    }

    /// create request to state to obtain median time past
    fn get_median_time_past(state: &Timeout<ZS>, block_hash: Option<block::Hash>) -> impl Future<Output = Result<DateTime<Utc>, TransactionError>>   {

        let state = state.clone();
    
        async move {
            if let Some(block_hash) = block_hash {
                let state = state.clone();
                // wait for the block to be added in the chain
                let query = state.oneshot(
                    zebra_state::Request::AwaitBlock(
                        block_hash
                    ));
                query.await?;
            }
            let query = state.oneshot(zebra_state::Request::GetMedianTimePast(block_hash));

            match query.await? {
                zebra_state::Response::MedianTimePast(Some(median_time_past)) => {
                    Ok(median_time_past)
                },
                zebra_state::Response::MedianTimePast(None) => { tracing::info!("cannot get MedianTimePast");  Err(TransactionError::KomodoMedianTimePastError)  }, 
                _ => unreachable!("Incorrect response from state service"),
            }
        }
    }

    /// validate transaction fee amount for too small or absurd values
    fn komodo_miner_fee_valid_for_mempool(rate_limiter: Arc<Mutex<FeeRateLimiter>>, min_relay_txfee: FeeRate, tx: &Transaction, tx_fee: Amount, check_low_fee: bool, reject_absurd_fee: bool) -> Result<(), TransactionError>   {
        let tx_size = tx.zcash_serialized_size().expect("structurally valid transaction must have size");
        
        if check_low_fee && tx_fee < min_relay_txfee.get_fee(tx_size)  {
            if let Ok(mut rate_limiter) = rate_limiter.clone().lock()  {
                if !rate_limiter.check_rate_limit(tx, Utc::now()) {
                    return Err(TransactionError::KomodoLowFeeLimit(tx.hash(), String::from("low txfee limit reached")));
                }
            }
            else {
                return Err(TransactionError::KomodoLowFeeLimit(tx.hash(), String::from("internal error: cannot lock limiter")));
            }
        }
        if reject_absurd_fee {
            let output_value = tx
                .outputs()
                .iter()
                .map(|o| o.value())
                .sum::<Result<Amount<NonNegative>, AmountError>>()
                .unwrap_or_else(|_| Amount::<NonNegative>::zero())
                .constrain::<NegativeAllowed>()
                .expect("conversion from NonNegative to NegativeAllowed is always valid");

            if tx_fee > (min_relay_txfee.get_fee(tx_size) * 10000 as u64).expect("valid min txfee") && tx_fee > (output_value / 19u64).expect("valid tx output value") {
                return Err(TransactionError::KomodoAbsurdFee(tx.hash(), tx_fee));
            }
        }
        
        Ok(())
    }

}

/// additional data needed for verification last transaction in block (added by Komodo)
type LastTxDataVerify = (Arc<Transaction>, CompactDifficulty, block::merkle::Root);

/// Specifies whether a transaction should be verified as part of a block or as
/// part of the mempool.
///
/// Transaction verification has slightly different consensus rules, depending on
/// whether the transaction is to be included in a block on in the mempool.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Request {
    /// Verify the supplied transaction as part of a block.
    Block {
        /// The transaction itself.
        transaction: Arc<Transaction>,
        /// Additional UTXOs which are known at the time of verification.
        known_utxos: Arc<HashMap<transparent::OutPoint, transparent::OrderedUtxo>>,
        /// The height of the block containing this transaction.
        height: block::Height,
        /// The time that the block was mined.
        time: DateTime<Utc>,
        /// previous block hash (komodo added)
        previous_hash: block::Hash,
        /// various data encapsulated in tuple, needed for last tx in the block verification, should be Some(...) only for last tx
        last_tx_verify_data: Option<LastTxDataVerify>,
    },
    /// Verify the supplied transaction as part of the mempool.
    ///
    /// Mempool transactions do not have any additional UTXOs.
    ///
    /// Note: coinbase transactions are invalid in the mempool
    Mempool {
        /// The transaction itself.
        transaction: UnminedTx,
        /// The height of the next block.
        ///
        /// The next block is the first block that could possibly contain a
        /// mempool transaction.
        height: block::Height,

        /// komodo added: check if tx fee is below limit (true for txns sent from remote nodes)
        check_low_fee: bool,

        /// komodo added: check if tx fee is too high (true for txns created locally)
        reject_absurd_fee: bool,
    },
}

/// The response type for the transaction verifier service.
/// Responses identify the transaction that was verified.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Response {
    /// A response to a block transaction verification request.
    Block {
        /// The witnessed transaction ID for this transaction.
        ///
        /// [`Response::Block`] responses can be uniquely identified by
        /// [`UnminedTxId::mined_id`], because the block's authorizing data root
        /// will be checked during contextual validation.
        tx_id: UnminedTxId,

        /// The miner fee for this transaction.
        ///
        /// `None` for coinbase transactions.
        ///
        /// # Consensus
        ///
        /// > The remaining value in the transparent transaction value pool
        /// > of a coinbase transaction is destroyed.
        ///
        /// <https://zips.z.cash/protocol/protocol.pdf#transactions>
        miner_fee: Option<Amount<NonNegative>>,

        /// The number of legacy signature operations in this transaction's
        /// transparent inputs and outputs.
        legacy_sigop_count: u64,

        /// komodo interest for tx
        interest: Option<Amount<NonNegative>>,
    },

    /// A response to a mempool transaction verification request.
    Mempool {
        /// The full content of the verified mempool transaction.
        /// Also contains the transaction fee and other associated fields.
        ///
        /// Mempool transactions always have a transaction fee,
        /// because coinbase transactions are rejected from the mempool.
        ///
        /// [`Response::Mempool`] responses are uniquely identified by the
        /// [`UnminedTxId`] variant for their transaction version.
        transaction: VerifiedUnminedTx,
    },
}

impl From<VerifiedUnminedTx> for Response {
    fn from(transaction: VerifiedUnminedTx) -> Self {
        Response::Mempool { transaction }
    }
}

impl Request {
    /// The transaction to verify that's in this request.
    pub fn transaction(&self) -> Arc<Transaction> {
        match self {
            Request::Block { transaction, .. } => transaction.clone(),
            Request::Mempool { transaction, .. } => transaction.transaction.clone(),
        }
    }

    /// The unverified mempool transaction, if this is a mempool request.
    pub fn into_mempool_transaction(self) -> Option<UnminedTx> {
        match self {
            Request::Block { .. } => None,
            Request::Mempool { transaction, .. } => Some(transaction),
        }
    }

    /// The unmined transaction ID for the transaction in this request.
    pub fn tx_id(&self) -> UnminedTxId {
        match self {
            // TODO: get the precalculated ID from the block verifier
            Request::Block { transaction, .. } => transaction.unmined_id(),
            Request::Mempool { transaction, .. } => transaction.id,
        }
    }

    /// The set of additional known unspent transaction outputs that's in this request.
    pub fn known_utxos(&self) -> Arc<HashMap<transparent::OutPoint, transparent::OrderedUtxo>> {
        match self {
            Request::Block { known_utxos, .. } => known_utxos.clone(),
            Request::Mempool { .. } => HashMap::new().into(),
        }
    }

    /// The height used to select the consensus rules for verifying this transaction.
    pub fn height(&self) -> block::Height {
        match self {
            Request::Block { height, .. } | Request::Mempool { height, .. } => *height,
        }
    }

    /// The block time used for lock time consensus rules validation.
    pub fn block_time(&self) -> Option<DateTime<Utc>> {
        match self {
            Request::Block { time, .. } => Some(*time),
            Request::Mempool { .. } => None,
        }
    }

    /// The network upgrade to consider for the verification.
    ///
    /// This is based on the block height from the request, and the supplied `network`.
    pub fn upgrade(&self, network: Network) -> NetworkUpgrade {
        NetworkUpgrade::current(network, self.height())
    }

    /// Returns true if the request is a mempool request.
    pub fn is_mempool(&self) -> bool {
        match self {
            Request::Block { .. } => false,
            Request::Mempool { .. } => true,
        }
    }

    /// Returns the coinbase if it's block request and it's passed.
    pub fn get_last_tx_verify_data(&self) -> Option<LastTxDataVerify> {
        match self {
            Request::Block { last_tx_verify_data, .. } => last_tx_verify_data.clone(),
            _ => None
        }
    }
}

impl Response {
    /// The verified mempool transaction, if this is a mempool response.
    pub fn into_mempool_transaction(self) -> Option<VerifiedUnminedTx> {
        match self {
            Response::Block { .. } => None,
            Response::Mempool { transaction, .. } => Some(transaction),
        }
    }

    /// The unmined transaction ID for the transaction in this response.
    pub fn tx_id(&self) -> UnminedTxId {
        match self {
            Response::Block { tx_id, .. } => *tx_id,
            Response::Mempool { transaction, .. } => transaction.transaction.id,
        }
    }

    /// The miner fee for the transaction in this response.
    ///
    /// Coinbase transactions do not have a miner fee.
    pub fn miner_fee(&self) -> Option<Amount<NonNegative>> {
        match self {
            Response::Block { miner_fee, .. } => *miner_fee,
            Response::Mempool { transaction, .. } => Some(transaction.miner_fee),
        }
    }

    /// The number of legacy transparent signature operations in this transaction's
    /// inputs and outputs.
    ///
    /// Zebra does not check the legacy sigop count for mempool transactions,
    /// because it is a standard rule (not a consensus rule).
    pub fn legacy_sigop_count(&self) -> Option<u64> {
        match self {
            Response::Block {
                legacy_sigop_count, ..
            } => Some(*legacy_sigop_count),
            Response::Mempool { .. } => None,
        }
    }

    /// Returns true if the request is a mempool request.
    pub fn is_mempool(&self) -> bool {
        match self {
            Response::Block { .. } => false,
            Response::Mempool { .. } => true,
        }
    }

    /// The komodo interest for the transaction in this response.
    ///
    /// Coinbase transactions do not have a miner fee.
    pub fn komodo_interest(&self) -> Option<Amount<NonNegative>> {
        match self {
            Response::Block { interest, .. } => *interest,
            Response::Mempool { transaction, .. } => Some(transaction.interest),
        }
    }
}

impl<ZS> Service<Request> for Verifier<ZS>
where
    ZS: Service<zs::Request, Response = zs::Response, Error = BoxError> + Send + Clone + 'static,
    ZS::Future: Send + 'static,
{
    type Response = Response;
    type Error = TransactionError;
    type Future =
        Pin<Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>>;

    fn poll_ready(&mut self, _cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        Poll::Ready(Ok(()))
    }

    // TODO: break up each chunk into its own method
    fn call(&mut self, req: Request) -> Self::Future {
        let script_verifier = self.script_verifier;
        let network = self.network;
        let state = self.state.clone();
        let min_relay_txfee = self.min_relay_txfee.clone();
        let rate_limiter = self.rate_limiter.clone();

        let tx = req.transaction();
        let tx_id = req.tx_id();
        let span = tracing::debug_span!("tx", ?tx_id);

        async move {
            tracing::trace!(?req);

            // Do basic checks first
            if let Some(block_time) = req.block_time() {
                check::is_final_tx_komodo(network, &tx, req.height(), block_time)?;
            }

            check::has_inputs_and_outputs(&tx)?;
            check::has_enough_orchard_flags(&tx)?;

            if req.is_mempool() && tx.is_coinbase() {
                return Err(TransactionError::CoinbaseInMempool);
            }
            if tx.is_coinbase() {
                check::coinbase_tx_no_prevout_joinsplit_spend(&tx)?;
            } else if !tx.is_valid_non_coinbase() {
                return Err(TransactionError::NonCoinbaseHasCoinbaseInput);
            }

            // Validate `nExpiryHeight` consensus rules
            if tx.is_coinbase() {
                check::coinbase_expiry_height(&req.height(), &tx, network)?;
            } else {
                check::non_coinbase_expiry_height(&req.height(), &tx)?;
            }

            // Consensus rule:
            //
            // > Either v_{pub}^{old} or v_{pub}^{new} MUST be zero.
            //
            // https://zips.z.cash/protocol/protocol.pdf#joinsplitdesc
            check::joinsplit_has_vpub_zero(&tx)?;

            // [Canopy onward]: `vpub_old` MUST be zero.
            // https://zips.z.cash/protocol/protocol.pdf#joinsplitdesc
            check::disabled_add_to_sprout_pool(&tx, req.height(), network)?;

            check::spend_conflicts(&tx)?;

            // Validate that tx locktime is not too early to prevent cheating with the beginning of komodo interest calculation period 
            let _ = match req.clone() {
                Request::Mempool { transaction, height, .. } => {
                    let query = Verifier::<ZS>::get_median_time_past(&state, None);
                    let cmp_time = query.await? + Duration::seconds(777); 
                    komodo_validate_interest_locktime(network, &transaction.transaction, height, cmp_time)?;
                    
                },
                Request::Block { transaction, height, time, previous_hash, .. } => {
                    let mut cmp_time = time; 
                    if NN::komodo_is_gap_after_second_block_allowed(network, &height) {
                        let query = Verifier::<ZS>::get_median_time_past(&state, Some(previous_hash));
                        let mtp = query.await?;
                        cmp_time = mtp + Duration::seconds(777); // HF22 - check interest validation against prev_block's MedianTimePast + 777 
                    }
                    komodo_validate_interest_locktime(network, &transaction, height, cmp_time)?;
                },
            };
                        
            let last_tip_blocktime = if !tx.is_coinbase() {
                // Request the tip block from the state and read its time to calculate komodo interest
                let query = Verifier::<ZS>::get_last_block_time(&state, &req);
                Some(query.await?)
            } else {
                None
            };
            
            // tx shouldn't have banned inputs
            check::tx_has_banned_inputs(&tx)?;

            // "The consensus rules applied to valueBalance, vShieldedOutput, and bindingSig
            // in non-coinbase transactions MUST also be applied to coinbase transactions."
            //
            // This rule is implicitly implemented during Sapling and Orchard verification,
            // because they do not distinguish between coinbase and non-coinbase transactions.
            //
            // Note: this rule originally applied to Sapling, but we assume it also applies to Orchard.
            //
            // https://zips.z.cash/zip-0213#specification

            // Load spent UTXOs from state.
            // TODO: Make this a method of `Request` and replace `tx.clone()` with `self.transaction()`?
            let (spent_utxos, spent_outputs) =
                Self::spent_utxos(tx.clone(), req.known_utxos(), req.is_mempool(), state).await?;

            // combined `komodo_check_deposit` and `komodo_checkopret` implementation (banned inputs is not part of the this check)
            if let Some(last_tx_verify_data)= req.get_last_tx_verify_data() {
                check::komodo_check_deposit_and_opret(&tx, &spent_utxos, &last_tx_verify_data, network, req.height())?;
            }

            let cached_ffi_transaction =
                Arc::new(CachedFfiTransaction::new(tx.clone(), spent_outputs));
            let async_checks = match tx.as_ref() {
                Transaction::V1 { .. } | Transaction::V2 { .. } | Transaction::V3 { .. } => {
                    tracing::debug!(?tx, "got transaction with wrong version");
                    return Err(TransactionError::WrongVersion);
                }
                Transaction::V4 {
                    joinsplit_data,
                    sapling_shielded_data,
                    ..
                } => Self::verify_v4_transaction(
                    &req,
                    network,
                    script_verifier,
                    cached_ffi_transaction.clone(),
                    joinsplit_data,
                    sapling_shielded_data,
                )?,
                Transaction::V5 {
                    sapling_shielded_data,
                    orchard_shielded_data,
                    ..
                } => Self::verify_v5_transaction(
                    &req,
                    network,
                    script_verifier,
                    cached_ffi_transaction.clone(),
                    sapling_shielded_data,
                    orchard_shielded_data,
                )?,
            };

            // If the Groth16 parameter download hangs,
            // Zebra will timeout here, waiting for the async checks.
            async_checks.check().await?;

            // Get the `value_balance` to calculate the transaction fee.
            let value_balance = tx.value_balance(network, &spent_utxos, req.height(), last_tip_blocktime);
            // also get dedicated interest value for checking it
            let value_interest = tx.komodo_interest_tx(network, &spent_utxos, req.height(), last_tip_blocktime);

            // Calculate the fee only for non-coinbase transactions.
            let mut miner_fee = None;
            if !tx.is_coinbase() {
                // TODO: deduplicate this code with remaining_transaction_value (#TODO: open ticket)
                miner_fee = match value_balance {
                    Ok(vb) => match vb.remaining_transaction_value() {
                        Ok(tx_rtv) => Some(tx_rtv),
                        Err(_) => { tracing::info!("remaining_transaction_value is error"); return Err(TransactionError::IncorrectFee) },
                    },
                    Err(_) => { tracing::info!("value_balance is error");  return Err(TransactionError::IncorrectFee) },
                };

                // for mempool check miner fee (too low or absurd), if requested
                if let Some(miner_fee) = miner_fee  { 
                    if let Request::Mempool { check_low_fee, reject_absurd_fee, .. } = req {
                        Self::komodo_miner_fee_valid_for_mempool(rate_limiter, min_relay_txfee, &tx, miner_fee.constrain().expect("miner fee conversion to NegativeAllowed must be okay"), check_low_fee, reject_absurd_fee)?;
                    }
                }
            }

            let rsp = match req {
                Request::Block { .. } => Response::Block {
                    tx_id,
                    miner_fee,
                    legacy_sigop_count: cached_ffi_transaction.legacy_sigop_count()?,
                    interest: Some(value_interest),
                },
                Request::Mempool { transaction, .. } => Response::Mempool {
                    transaction: VerifiedUnminedTx::new(
                        transaction,
                        miner_fee // unwrap_or(Amount::zero()),
                            .expect("unexpected mempool coinbase transaction: should have already rejected"),
                        value_interest,
                    ),
                },
            };

            Ok(rsp)
        }
        .instrument(span)
        .boxed()
    }
}

impl<ZS> Verifier<ZS>
where
    ZS: Service<zs::Request, Response = zs::Response, Error = BoxError> + Send + Clone + 'static,
    ZS::Future: Send + 'static,
{
    /// Get the UTXOs that are being spent by the given transaction.
    ///
    /// `known_utxos` are additional UTXOs known at the time of validation (i.e.
    /// from previous transactions in the block).
    ///
    /// Returns a tuple with a OutPoint -> Utxo map, and a vector of Outputs
    /// in the same order as the matching inputs in the transaction.
    async fn spent_utxos(
        tx: Arc<Transaction>,
        known_utxos: Arc<HashMap<transparent::OutPoint, OrderedUtxo>>,
        is_mempool: bool,
        state: Timeout<ZS>,
    ) -> Result<
        (
            HashMap<transparent::OutPoint, transparent::Utxo>,
            Vec<transparent::Output>,
        ),
        TransactionError,
    > {
        let inputs = tx.inputs();
        let mut spent_utxos = HashMap::new();
        let mut spent_outputs = Vec::new();
        for input in inputs {
            if let transparent::Input::PrevOut { outpoint, .. } = input {
                tracing::trace!("awaiting outpoint lookup");
                let utxo = if let Some(output) = known_utxos.get(outpoint) {
                    tracing::trace!("UXTO in known_utxos, discarding query");
                    output.utxo.clone()
                } else if is_mempool {
                    let query = state
                        .clone()
                        .oneshot(zs::Request::UnspentBestChainUtxo(*outpoint));
                    if let zebra_state::Response::UnspentBestChainUtxo(utxo) = query.await? {
                        utxo.ok_or(TransactionError::TransparentInputNotFound)?
                    } else {
                        unreachable!("UnspentBestChainUtxo always responds with Option<Utxo>")
                    }
                } else {
                    let query = state
                        .clone()
                        .oneshot(zebra_state::Request::AwaitUtxo(*outpoint));
                    if let zebra_state::Response::Utxo(utxo) = query.await? {
                        utxo
                    } else {
                        unreachable!("AwaitUtxo always responds with Utxo")
                    }
                };
                tracing::trace!(?utxo, "got UTXO");
                spent_outputs.push(utxo.output.clone());
                spent_utxos.insert(*outpoint, utxo);
            } else {
                continue;
            }
        }
        Ok((spent_utxos, spent_outputs))
    }

    /// Verify a V4 transaction.
    ///
    /// Returns a set of asynchronous checks that must all succeed for the transaction to be
    /// considered valid. These checks include:
    ///
    /// - transparent transfers
    /// - sprout shielded data
    /// - sapling shielded data
    ///
    /// The parameters of this method are:
    ///
    /// - the `request` to verify (that contains the transaction and other metadata, see [`Request`]
    ///   for more information)
    /// - the `network` to consider when verifying
    /// - the `script_verifier` to use for verifying the transparent transfers
    /// - the prepared `cached_ffi_transaction` used by the script verifier
    /// - the Sprout `joinsplit_data` shielded data in the transaction
    /// - the `sapling_shielded_data` in the transaction
    fn verify_v4_transaction(
        request: &Request,
        network: Network,
        script_verifier: script::Verifier,
        cached_ffi_transaction: Arc<CachedFfiTransaction>,
        joinsplit_data: &Option<transaction::JoinSplitData<Groth16Proof>>,
        sapling_shielded_data: &Option<sapling::ShieldedData<sapling::PerSpendAnchor>>,
    ) -> Result<AsyncChecks, TransactionError> {
        let tx = request.transaction();
        let upgrade = request.upgrade(network);

        Self::verify_v4_transaction_network_upgrade(&tx, upgrade)?;

        let shielded_sighash = tx.sighash(
            upgrade,
            HashType::ALL,
            cached_ffi_transaction.all_previous_outputs(),
            None,
        );

        Ok(Self::verify_transparent_inputs_and_outputs(
            request,
            network,
            script_verifier,
            cached_ffi_transaction,
        )?
        .and(Self::verify_sprout_shielded_data(
            joinsplit_data,
            &shielded_sighash,
        )?)
        .and(Self::verify_sapling_shielded_data(
            sapling_shielded_data,
            &shielded_sighash,
        )?))
    }

    /// Verifies if a V4 `transaction` is supported by `network_upgrade`.
    fn verify_v4_transaction_network_upgrade(
        transaction: &Transaction,
        network_upgrade: NetworkUpgrade,
    ) -> Result<(), TransactionError> {
        match network_upgrade {
            // Supports V4 transactions
            //
            // # Consensus
            //
            // > [Sapling to Canopy inclusive, pre-NU5] The transaction version number MUST be 4,
            // > and the version group ID MUST be 0x892F2085.
            //
            // > [NU5 onward] The transaction version number MUST be 4 or 5.
            // > If the transaction version number is 4 then the version group ID MUST be 0x892F2085.
            // > If the transaction version number is 5 then the version group ID MUST be 0x26A7270A.
            //
            // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
            //
            // Note: Here we verify the transaction version number of the above two rules, the group
            // id is checked in zebra-chain crate, in the transaction serialize.
            NetworkUpgrade::Sapling
            | NetworkUpgrade::Blossom
            | NetworkUpgrade::Heartwood
            | NetworkUpgrade::Canopy
            | NetworkUpgrade::Nu5 => Ok(()),

            // Does not support V4 transactions
            NetworkUpgrade::Genesis
            | NetworkUpgrade::BeforeOverwinter
            | NetworkUpgrade::Overwinter => Err(TransactionError::UnsupportedByNetworkUpgrade(
                transaction.version(),
                network_upgrade,
            )),
        }
    }

    /// Verify a V5 transaction.
    ///
    /// Returns a set of asynchronous checks that must all succeed for the transaction to be
    /// considered valid. These checks include:
    ///
    /// - transaction support by the considered network upgrade (see [`Request::upgrade`])
    /// - transparent transfers
    /// - sapling shielded data (TODO)
    /// - orchard shielded data (TODO)
    ///
    /// The parameters of this method are:
    ///
    /// - the `request` to verify (that contains the transaction and other metadata, see [`Request`]
    ///   for more information)
    /// - the `network` to consider when verifying
    /// - the `script_verifier` to use for verifying the transparent transfers
    /// - the prepared `cached_ffi_transaction` used by the script verifier
    /// - the sapling shielded data of the transaction, if any
    /// - the orchard shielded data of the transaction, if any
    fn verify_v5_transaction(
        request: &Request,
        network: Network,
        script_verifier: script::Verifier,
        cached_ffi_transaction: Arc<CachedFfiTransaction>,
        sapling_shielded_data: &Option<sapling::ShieldedData<sapling::SharedAnchor>>,
        orchard_shielded_data: &Option<orchard::ShieldedData>,
    ) -> Result<AsyncChecks, TransactionError> {
        let transaction = request.transaction();
        let upgrade = request.upgrade(network);

        Self::verify_v5_transaction_network_upgrade(&transaction, upgrade)?;

        let shielded_sighash = transaction.sighash(
            upgrade,
            HashType::ALL,
            cached_ffi_transaction.all_previous_outputs(),
            None,
        );

        Ok(Self::verify_transparent_inputs_and_outputs(
            request,
            network,
            script_verifier,
            cached_ffi_transaction,
        )?
        .and(Self::verify_sapling_shielded_data(
            sapling_shielded_data,
            &shielded_sighash,
        )?)
        .and(Self::verify_orchard_shielded_data(
            orchard_shielded_data,
            &shielded_sighash,
        )?))

        // TODO:
        // - verify orchard shielded pool (ZIP-224) (#2105)
        // - shielded input and output limits? (#2379)
    }

    /// Verifies if a V5 `transaction` is supported by `network_upgrade`.
    fn verify_v5_transaction_network_upgrade(
        transaction: &Transaction,
        network_upgrade: NetworkUpgrade,
    ) -> Result<(), TransactionError> {
        match network_upgrade {
            // Supports V5 transactions
            //
            // # Consensus
            //
            // > [NU5 onward] The transaction version number MUST be 4 or 5.
            // > If the transaction version number is 4 then the version group ID MUST be 0x892F2085.
            // > If the transaction version number is 5 then the version group ID MUST be 0x26A7270A.
            //
            // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
            //
            // Note: Here we verify the transaction version number of the above rule, the group
            // id is checked in zebra-chain crate, in the transaction serialize.
            NetworkUpgrade::Nu5 => Ok(()),

            // Does not support V5 transactions
            NetworkUpgrade::Genesis
            | NetworkUpgrade::BeforeOverwinter
            | NetworkUpgrade::Overwinter
            | NetworkUpgrade::Sapling
            | NetworkUpgrade::Blossom
            | NetworkUpgrade::Heartwood
            | NetworkUpgrade::Canopy => Err(TransactionError::UnsupportedByNetworkUpgrade(
                transaction.version(),
                network_upgrade,
            )),
        }
    }

    /// Verifies if a transaction's transparent inputs are valid using the provided
    /// `script_verifier` and `cached_ffi_transaction`.
    ///
    /// Returns script verification responses via the `utxo_sender`.
    fn verify_transparent_inputs_and_outputs(
        request: &Request,
        network: Network,
        script_verifier: script::Verifier,
        cached_ffi_transaction: Arc<CachedFfiTransaction>,
    ) -> Result<AsyncChecks, TransactionError> {
        let transaction = request.transaction();

        if transaction.is_coinbase() {
            // The script verifier only verifies PrevOut inputs and their corresponding UTXOs.
            // Coinbase transactions don't have any PrevOut inputs.
            Ok(AsyncChecks::new())
        } else {
            // feed all of the inputs to the script verifier
            // the script_verifier also checks transparent sighashes, using its own implementation
            let inputs = transaction.inputs();
            let upgrade = request.upgrade(network);

            let script_checks = (0..inputs.len())
                .into_iter()
                .map(move |input_index| {
                    let request = script::Request {
                        upgrade,
                        cached_ffi_transaction: cached_ffi_transaction.clone(),
                        input_index,
                    };

                    script_verifier.oneshot(request)
                })
                .collect();

            Ok(script_checks)
        }
    }

    /// Verifies a transaction's Sprout shielded join split data.
    fn verify_sprout_shielded_data(
        joinsplit_data: &Option<transaction::JoinSplitData<Groth16Proof>>,
        shielded_sighash: &SigHash,
    ) -> Result<AsyncChecks, TransactionError> {
        let mut checks = AsyncChecks::new();

        if let Some(joinsplit_data) = joinsplit_data {
            for joinsplit in joinsplit_data.joinsplits() {
                // # Consensus
                //
                // > The proof π_ZKJoinSplit MUST be valid given a
                // > primary input formed from the relevant other fields and h_{Sig}
                //
                // https://zips.z.cash/protocol/protocol.pdf#joinsplitdesc
                //
                // Queue the verification of the Groth16 spend proof
                // for each JoinSplit description while adding the
                // resulting future to our collection of async
                // checks that (at a minimum) must pass for the
                // transaction to verify.
                checks.push(primitives::groth16::JOINSPLIT_VERIFIER.oneshot(
                    DescriptionWrapper(&(joinsplit, &joinsplit_data.pub_key)).try_into()?,
                ));
            }

            // # Consensus
            //
            // > If effectiveVersion ≥ 2 and nJoinSplit > 0, then:
            // > - joinSplitPubKey MUST be a valid encoding of an Ed25519 validating key
            // > - joinSplitSig MUST represent a valid signature under
            //     joinSplitPubKey of dataToBeSigned, as defined in § 4.11
            //
            // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
            //
            // The `if` part is indirectly enforced, since the `joinsplit_data`
            // is only parsed if those conditions apply in
            // [`Transaction::zcash_deserialize`].
            //
            // The valid encoding is defined in
            //
            // > A valid Ed25519 validating key is defined as a sequence of 32
            // > bytes encoding a point on the Ed25519 curve
            //
            // https://zips.z.cash/protocol/protocol.pdf#concreteed25519
            //
            // which is enforced during signature verification, in both batched
            // and single verification, when decompressing the encoded point.
            //
            // Queue the validation of the JoinSplit signature while
            // adding the resulting future to our collection of
            // async checks that (at a minimum) must pass for the
            // transaction to verify.
            //
            // https://zips.z.cash/protocol/protocol.pdf#sproutnonmalleability
            // https://zips.z.cash/protocol/protocol.pdf#txnencodingandconsensus
            let ed25519_verifier = primitives::ed25519::VERIFIER.clone();
            let ed25519_item =
                (joinsplit_data.pub_key, joinsplit_data.sig, shielded_sighash).into();

            checks.push(ed25519_verifier.oneshot(ed25519_item));
        }

        Ok(checks)
    }

    /// Verifies a transaction's Sapling shielded data.
    fn verify_sapling_shielded_data<A>(
        sapling_shielded_data: &Option<sapling::ShieldedData<A>>,
        shielded_sighash: &SigHash,
    ) -> Result<AsyncChecks, TransactionError>
    where
        A: sapling::AnchorVariant + Clone,
        sapling::Spend<sapling::PerSpendAnchor>: From<(sapling::Spend<A>, A::Shared)>,
    {
        let mut async_checks = AsyncChecks::new();

        if let Some(sapling_shielded_data) = sapling_shielded_data {
            for spend in sapling_shielded_data.spends_per_anchor() {
                // # Consensus
                //
                // > The proof π_ZKSpend MUST be valid
                // > given a primary input formed from the other
                // > fields except spendAuthSig.
                //
                // https://zips.z.cash/protocol/protocol.pdf#spenddesc
                //
                // Queue the verification of the Groth16 spend proof
                // for each Spend description while adding the
                // resulting future to our collection of async
                // checks that (at a minimum) must pass for the
                // transaction to verify.
                async_checks.push(
                    primitives::groth16::SPEND_VERIFIER
                        .clone()
                        .oneshot(DescriptionWrapper(&spend).try_into()?),
                );

                // # Consensus
                //
                // > The spend authorization signature
                // > MUST be a valid SpendAuthSig signature over
                // > SigHash using rk as the validating key.
                //
                // This is validated by the verifier.
                //
                // > [NU5 onward] As specified in § 5.4.7 ‘RedDSA, RedJubjub,
                // > and RedPallas’ on p. 88, the validation of the 𝑅
                // > component of the signature changes to prohibit non-canonical encodings.
                //
                // This is validated by the verifier, inside the `redjubjub` crate.
                // It calls [`jubjub::AffinePoint::from_bytes`] to parse R and
                // that enforces the canonical encoding.
                //
                // https://zips.z.cash/protocol/protocol.pdf#spenddesc
                //
                // Queue the validation of the RedJubjub spend
                // authorization signature for each Spend
                // description while adding the resulting future to
                // our collection of async checks that (at a
                // minimum) must pass for the transaction to verify.
                async_checks.push(
                    primitives::redjubjub::VERIFIER
                        .clone()
                        .oneshot((spend.rk.into(), spend.spend_auth_sig, shielded_sighash).into()),
                );
            }

            for output in sapling_shielded_data.outputs() {
                // # Consensus
                //
                // > The proof π_ZKOutput MUST be
                // > valid given a primary input formed from the other
                // > fields except C^enc and C^out.
                //
                // https://zips.z.cash/protocol/protocol.pdf#outputdesc
                //
                // Queue the verification of the Groth16 output
                // proof for each Output description while adding
                // the resulting future to our collection of async
                // checks that (at a minimum) must pass for the
                // transaction to verify.
                async_checks.push(
                    primitives::groth16::OUTPUT_VERIFIER
                        .clone()
                        .oneshot(DescriptionWrapper(output).try_into()?),
                );
            }

            // # Consensus
            //
            // > The Spend transfers and Action transfers of a transaction MUST be
            // > consistent with its vbalanceSapling value as specified in § 4.13
            // > ‘Balance and Binding Signature (Sapling)’.
            //
            // https://zips.z.cash/protocol/protocol.pdf#spendsandoutputs
            //
            // > [Sapling onward] If effectiveVersion ≥ 4 and
            // > nSpendsSapling + nOutputsSapling > 0, then:
            // > – let bvk^{Sapling} and SigHash be as defined in § 4.13;
            // > – bindingSigSapling MUST represent a valid signature under the
            // >   transaction binding validating key bvk Sapling of SigHash —
            // >   i.e. BindingSig^{Sapling}.Validate_{bvk^{Sapling}}(SigHash, bindingSigSapling ) = 1.
            //
            // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
            //
            // This is validated by the verifier. The `if` part is indirectly
            // enforced, since the `sapling_shielded_data` is only parsed if those
            // conditions apply in [`Transaction::zcash_deserialize`].
            //
            // >   [NU5 onward] As specified in § 5.4.7, the validation of the 𝑅 component
            // >   of the signature changes to prohibit non-canonical encodings.
            //
            // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
            //
            // This is validated by the verifier, inside the `redjubjub` crate.
            // It calls [`jubjub::AffinePoint::from_bytes`] to parse R and
            // that enforces the canonical encoding.

            let bvk = sapling_shielded_data.binding_verification_key();

            async_checks.push(
                primitives::redjubjub::VERIFIER
                    .clone()
                    .oneshot((bvk, sapling_shielded_data.binding_sig, &shielded_sighash).into()),
            );
        }

        Ok(async_checks)
    }

    /// Verifies a transaction's Orchard shielded data.
    fn verify_orchard_shielded_data(
        orchard_shielded_data: &Option<orchard::ShieldedData>,
        shielded_sighash: &SigHash,
    ) -> Result<AsyncChecks, TransactionError> {
        let mut async_checks = AsyncChecks::new();

        if let Some(orchard_shielded_data) = orchard_shielded_data {
            // # Consensus
            //
            // > The proof 𝜋 MUST be valid given a primary input (cv, rt^{Orchard},
            // > nf, rk, cm_x, enableSpends, enableOutputs)
            //
            // https://zips.z.cash/protocol/protocol.pdf#actiondesc
            //
            // Unlike Sapling, Orchard shielded transactions have a single
            // aggregated Halo2 proof per transaction, even with multiple
            // Actions in one transaction. So we queue it for verification
            // only once instead of queuing it up for every Action description.
            async_checks.push(
                primitives::halo2::VERIFIER
                    .clone()
                    .oneshot(primitives::halo2::Item::from(orchard_shielded_data)),
            );

            for authorized_action in orchard_shielded_data.actions.iter().cloned() {
                let (action, spend_auth_sig) = authorized_action.into_parts();

                // # Consensus
                //
                // > - Let SigHash be the SIGHASH transaction hash of this transaction, not
                // >   associated with an input, as defined in § 4.10 using SIGHASH_ALL.
                // > - The spend authorization signature MUST be a valid SpendAuthSig^{Orchard}
                // >   signature over SigHash using rk as the validating key — i.e.
                // >   SpendAuthSig^{Orchard}.Validate_{rk}(SigHash, spendAuthSig) = 1.
                // >   As specified in § 5.4.7, validation of the 𝑅 component of the
                // >   signature prohibits non-canonical encodings.
                //
                // https://zips.z.cash/protocol/protocol.pdf#actiondesc
                //
                // This is validated by the verifier, inside the [`primitives::redpallas`] module.
                // It calls [`pallas::Affine::from_bytes`] to parse R and
                // that enforces the canonical encoding.
                //
                // Queue the validation of the RedPallas spend
                // authorization signature for each Action
                // description while adding the resulting future to
                // our collection of async checks that (at a
                // minimum) must pass for the transaction to verify.
                async_checks.push(
                    primitives::redpallas::VERIFIER
                        .clone()
                        .oneshot((action.rk, spend_auth_sig, &shielded_sighash).into()),
                );
            }

            let bvk = orchard_shielded_data.binding_verification_key();

            // # Consensus
            //
            // > The Action transfers of a transaction MUST be consistent with
            // > its v balanceOrchard value as specified in § 4.14.
            //
            // https://zips.z.cash/protocol/protocol.pdf#actions
            //
            // > [NU5 onward] If effectiveVersion ≥ 5 and nActionsOrchard > 0, then:
            // > – let bvk^{Orchard} and SigHash be as defined in § 4.14;
            // > – bindingSigOrchard MUST represent a valid signature under the
            // >   transaction binding validating key bvk^{Orchard} of SigHash —
            // >   i.e. BindingSig^{Orchard}.Validate_{bvk^{Orchard}}(SigHash, bindingSigOrchard) = 1.
            //
            // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
            //
            // This is validated by the verifier. The `if` part is indirectly
            // enforced, since the `orchard_shielded_data` is only parsed if those
            // conditions apply in [`Transaction::zcash_deserialize`].
            //
            // >   As specified in § 5.4.7, validation of the 𝑅 component of the signature
            // >   prohibits non-canonical encodings.
            //
            // https://zips.z.cash/protocol/protocol.pdf#txnconsensus
            //
            // This is validated by the verifier, inside the `redpallas` crate.
            // It calls [`pallas::Affine::from_bytes`] to parse R and
            // that enforces the canonical encoding.

            async_checks.push(
                primitives::redpallas::VERIFIER
                    .clone()
                    .oneshot((bvk, orchard_shielded_data.binding_sig, &shielded_sighash).into()),
            );
        }

        Ok(async_checks)
    }
}

/// A set of unordered asynchronous checks that should succeed.
///
/// A wrapper around [`FuturesUnordered`] with some auxiliary methods.
struct AsyncChecks(FuturesUnordered<Pin<Box<dyn Future<Output = Result<(), BoxError>> + Send>>>);

impl AsyncChecks {
    /// Create an empty set of unordered asynchronous checks.
    pub fn new() -> Self {
        AsyncChecks(FuturesUnordered::new())
    }

    /// Push a check into the set.
    pub fn push(&mut self, check: impl Future<Output = Result<(), BoxError>> + Send + 'static) {
        self.0.push(check.boxed());
    }

    /// Push a set of checks into the set.
    ///
    /// This method can be daisy-chained.
    pub fn and(mut self, checks: AsyncChecks) -> Self {
        self.0.extend(checks.0);
        self
    }

    /// Wait until all checks in the set finish.
    ///
    /// If any of the checks fail, this method immediately returns the error and cancels all other
    /// checks by dropping them.
    async fn check(mut self) -> Result<(), BoxError> {
        // Wait for all asynchronous checks to complete
        // successfully, or fail verification if they error.
        while let Some(check) = self.0.next().await {
            tracing::trace!(?check, remaining = self.0.len());
            check?;
        }

        Ok(())
    }
}

impl<F> FromIterator<F> for AsyncChecks
where
    F: Future<Output = Result<(), BoxError>> + Send + 'static,
{
    fn from_iter<I>(iterator: I) -> Self
    where
        I: IntoIterator<Item = F>,
    {
        AsyncChecks(iterator.into_iter().map(FutureExt::boxed).collect())
    }
}

/// validate tx lock time so it has not stayed in mempool for a long time 
/// to prevent cheating with the tx lock time, which is actually the start of interest period, to get extra interest value
pub fn komodo_validate_interest_locktime(network: Network, tx: &Transaction, tx_height: block::Height, cmp_time: DateTime<Utc>) -> Result<(), TransactionError> {

    if let Some(lock_time) = tx.raw_lock_time() {       // in komodo we should not use zcash's special lock_time()
        if let LockTime::Time(lock_time) = lock_time {  
            if NN::komodo_interest_validate_locktime_active(network, &tx_height)  {
                let mut cmp_time_adj = cmp_time;
                if NN::komodo_interest_adjust_max_mempool_time_active(network, &tx_height)  {
                    cmp_time_adj -= Duration::seconds(16000);
                }
                if lock_time < cmp_time_adj - Duration::seconds(KOMODO_MAXMEMPOOLTIME)   {
                    tracing::info!("komodo_validate_interest_locktime reject tx {:?} for ht={:?} too early secs {} locktime {} cmp_time {}\n", tx.hash(), tx_height, (lock_time - (cmp_time_adj - Duration::seconds(KOMODO_MAXMEMPOOLTIME))), lock_time.timestamp(), cmp_time_adj.timestamp());
                    return Err(TransactionError::KomodoTxLockTimeTooEarly(lock_time.timestamp(), tx_height));
                }
                tracing::debug!("komodo_validate_interest_locktime accept tx {:?} for ht={:?} locktime-maxtime secs {} locktime {} cmp_time {}\n", tx.hash(), tx_height, (lock_time - (cmp_time_adj - Duration::seconds(KOMODO_MAXMEMPOOLTIME))), lock_time.timestamp(), cmp_time_adj.timestamp());
            }
        }
    }
    Ok(())
}


