//! Transaction checks.
//!
//! Code in this file can freely assume that no pre-V4 transactions are present.

use std::{borrow::Cow, collections::HashSet, convert::TryFrom, hash::Hash};

use chrono::{DateTime, Utc};

use zebra_chain::{
    amount::{Amount, NonNegative},
    block::Height,
    orchard::Flags,
    parameters::{Network, NetworkUpgrade},
    primitives::zcash_note_encryption,
    transaction::{LockTime, Transaction}, transparent,
};

use zebra_chain::komodo_hardfork::NN;

use crate::error::TransactionError;

/// Checks if the transaction's lock time allows this transaction to be included in a block.
///
/// Consensus rule:
///
/// > The transaction must be finalized: either its locktime must be in the past (or less
/// > than or equal to the current block height), or all of its sequence numbers must be
/// > 0xffffffff.
///
/// [`Transaction::lock_time`] validates the transparent input sequence numbers, returning [`None`]
/// if they indicate that the transaction is finalized by them. Otherwise, this function validates
/// if the lock time is in the past.
pub fn lock_time_has_passed(
    network: Network,
    tx: &Transaction,
    block_height: Height,
    block_time: DateTime<Utc>,
) -> Result<(), TransactionError> {
    match tx.lock_time() {
        Some(LockTime::Height(unlock_height)) => {
            // > The transaction can be added to any block which has a greater height.
            // The Bitcoin documentation is wrong or outdated here,
            // so this code is based on the `zcashd` implementation at:
            // https://github.com/zcash/zcash/blob/1a7c2a3b04bcad6549be6d571bfdff8af9a2c814/src/main.cpp#L722
            if block_height > unlock_height {
                Ok(())
            } else {
                Err(TransactionError::LockedUntilAfterBlockHeight(unlock_height))
            }
        }
        Some(LockTime::Time(unlock_time)) => {
            // > The transaction can be added to any block whose block time is greater than the locktime.
            // https://developer.bitcoin.org/devguide/transactions.html#locktime-and-sequence-number
            if block_time > unlock_time {
                Ok(())
            } else {
                Err(TransactionError::LockedUntilAfterBlockTime(unlock_time))
            }
        }
        None => Ok(()),
    }
}


/// This function should match `komodod` function here: https://github.com/KomodoPlatform/komodo/blob/master/src/main.cpp#L924
///
/// Main rules:
///
/// 1. If nLockTime tx field set to 0 - it's final.
/// 2. If nLockTime < nBlockHeight or nBlockTime (consider "apples are apples", mean that nLockTime represented the Height compare only
///    with Height, and nLockTime represented Time, compare only with Time values) - tx is also considered to be final.
/// 3. If all vins have 0xFFFFFFFF sequence tx is considered to be final regardless of nLockTime fields.
/// 4. And finally, there is a some historical Komodo exceptions for vins with sequence == 0xFFFFFFFE, depends on komodo_hardfork_active.

pub fn is_final_tx_komodo(
    network: Network,
    tx: &Transaction,
    block_height: Height,
    block_time: DateTime<Utc>,
) -> Result<(), TransactionError> {

    if let Some(lock_time) = tx.raw_lock_time() {

        if lock_time == LockTime::unlocked() {
            return Ok(());
        }

        match lock_time {
            LockTime::Height(unlock_height) => {
                if unlock_height < block_height {
                    return Ok(())
                }
            },
            LockTime::Time(unlock_time) => {
                if unlock_time < block_time {
                    return Ok(())
                }
            }
        }

        // in `komodod` HF check is implemented in komodo_hardfork_active function, but for KMD coin
        // this check is implemented like chainActive.Height() > nDecemberHardforkHeight), where
        // chain.Tip() ? chain.Tip()->nHeight : -1. In other words, it always compared with the
        // height of a last tip (!), i.e. one block before block being validated.

        let validation_height = block_height - 1;
        let hf_active = if let Some(ht) = validation_height {
            NN::komodo_s1_december_hardfork_active(network, &ht)
        } else {
            false
        };

        // now let's analyze tx vins
        let tx_is_non_final = tx
            .inputs()
            .iter()
            .map(transparent::Input::sequence)
            .any(|sequence_number| {

                // this closure should return true if vin is "non-final" (it's sequence != u32::MAX
                // and it doesn't satisfied other komodo exceptions)

                // f_exception is true, when nLockTime > (nBlockTime or nBlockHeight)
                let f_exception = match lock_time {
                    LockTime::Height(unlock_height) => unlock_height > block_height,
                    LockTime::Time(unlock_time) => unlock_time > block_time
                };

                if !hf_active && sequence_number == u32::MAX - 1 && f_exception
                {
                    false
                } else if hf_active && sequence_number == u32::MAX - 1 && !f_exception
                {
                    false
                } else {
                    sequence_number != u32::MAX
                }
            });

        if tx_is_non_final {
            return match lock_time {
                LockTime::Height(unlock_height) => Err(TransactionError::LockedUntilAfterBlockHeight(unlock_height)),
                LockTime::Time(unlock_time) => Err(TransactionError::LockedUntilAfterBlockTime(unlock_time)),
            };
        }
    }

    // if tx.raw_lock_time() returned None it means that tx.lock_time == LockTime::unlocked(), i.e.
    // LockTime::Height(block::Height(0)), and in this case tx also considered to be final.

    Ok(())

}

/// Checks that the transaction has inputs and outputs.
///
/// # Consensus
///
/// For `Transaction::V4`:
///
/// > [Sapling onward] If effectiveVersion < 5, then at least one of
/// > tx_in_count, nSpendsSapling, and nJoinSplit MUST be nonzero.
///
/// > [Sapling onward] If effectiveVersion < 5, then at least one of
/// > tx_out_count, nOutputsSapling, and nJoinSplit MUST be nonzero.
///
/// For `Transaction::V5`:
///
/// > [NU5 onward] If effectiveVersion >= 5 then this condition MUST hold:
/// > tx_in_count > 0 or nSpendsSapling > 0 or (nActionsOrchard > 0 and enableSpendsOrchard = 1).
///
/// > [NU5 onward] If effectiveVersion >= 5 then this condition MUST hold:
/// > tx_out_count > 0 or nOutputsSapling > 0 or (nActionsOrchard > 0 and enableOutputsOrchard = 1).
///
/// <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
///
/// This check counts both `Coinbase` and `PrevOut` transparent inputs.
pub fn has_inputs_and_outputs(tx: &Transaction) -> Result<(), TransactionError> {
    if !tx.has_transparent_or_shielded_inputs() {
        Err(TransactionError::NoInputs)
    } else if !tx.has_transparent_or_shielded_outputs() {
        Err(TransactionError::NoOutputs)
    } else {
        Ok(())
    }
}

/// Checks that the transaction has enough orchard flags.
///
/// # Consensus
///
/// For `Transaction::V5` only:
///
/// > [NU5 onward] If effectiveVersion >= 5 and nActionsOrchard > 0, then at least one of enableSpendsOrchard and enableOutputsOrchard MUST be 1.
///
/// <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
pub fn has_enough_orchard_flags(tx: &Transaction) -> Result<(), TransactionError> {
    if !tx.has_enough_orchard_flags() {
        return Err(TransactionError::NotEnoughFlags);
    }
    Ok(())
}

/// Check that a coinbase transaction has no PrevOut inputs, JoinSplits, or spends.
///
/// # Consensus
///
/// > A coinbase transaction MUST NOT have any JoinSplit descriptions.
///
/// > A coinbase transaction MUST NOT have any Spend descriptions.
///
/// > [NU5 onward] In a version 5 coinbase transaction, the enableSpendsOrchard flag MUST be 0.
///
/// This check only counts `PrevOut` transparent inputs.
///
/// > [Pre-Heartwood] A coinbase transaction also MUST NOT have any Output descriptions.
///
/// Zebra does not validate this last rule explicitly because we checkpoint until Canopy activation.
///
/// <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
pub fn coinbase_tx_no_prevout_joinsplit_spend(tx: &Transaction) -> Result<(), TransactionError> {
    if tx.is_coinbase() {
        if tx.joinsplit_count() > 0 {
            return Err(TransactionError::CoinbaseHasJoinSplit);
        } else if tx.sapling_spends_per_anchor().count() > 0 {
            return Err(TransactionError::CoinbaseHasSpend);
        }

        if let Some(orchard_shielded_data) = tx.orchard_shielded_data() {
            if orchard_shielded_data.flags.contains(Flags::ENABLE_SPENDS) {
                return Err(TransactionError::CoinbaseHasEnableSpendsOrchard);
            }
        }
    }

    Ok(())
}

/// Check if JoinSplits in the transaction have one of its v_{pub} values equal
/// to zero.
///
/// <https://zips.z.cash/protocol/protocol.pdf#joinsplitdesc>
pub fn joinsplit_has_vpub_zero(tx: &Transaction) -> Result<(), TransactionError> {
    let zero = Amount::<NonNegative>::try_from(0).expect("an amount of 0 is always valid");

    let vpub_pairs = tx
        .output_values_to_sprout()
        .zip(tx.input_values_from_sprout());
    for (vpub_old, vpub_new) in vpub_pairs {
        // # Consensus
        //
        // > Either v_{pub}^{old} or v_{pub}^{new} MUST be zero.
        //
        // https://zips.z.cash/protocol/protocol.pdf#joinsplitdesc
        if *vpub_old != zero && *vpub_new != zero {
            return Err(TransactionError::BothVPubsNonZero);
        }
    }

    Ok(())
}

/// Check if a transaction is adding to the sprout pool after Canopy
/// network upgrade given a block height and a network.
///
/// <https://zips.z.cash/zip-0211>
/// <https://zips.z.cash/protocol/protocol.pdf#joinsplitdesc>
pub fn disabled_add_to_sprout_pool(
    tx: &Transaction,
    height: Height,
    network: Network,
) -> Result<(), TransactionError> {
    let canopy_activation_height = NetworkUpgrade::Canopy
        .activation_height(network)
        .expect("Canopy activation height must be present for both networks");

    // # Consensus
    //
    // > [Canopy onward]: `vpub_old` MUST be zero.
    //
    // https://zips.z.cash/protocol/protocol.pdf#joinsplitdesc
    if height >= canopy_activation_height {
        let zero = Amount::<NonNegative>::try_from(0).expect("an amount of 0 is always valid");

        let tx_sprout_pool = tx.output_values_to_sprout();
        for vpub_old in tx_sprout_pool {
            if *vpub_old != zero {
                return Err(TransactionError::DisabledAddToSproutPool);
            }
        }
    }

    Ok(())
}

/// Check if a transaction has any internal spend conflicts.
///
/// An internal spend conflict happens if the transaction spends a UTXO more than once or if it
/// reveals a nullifier more than once.
///
/// Consensus rules:
///
/// "each output of a particular transaction
/// can only be used as an input once in the block chain.
/// Any subsequent reference is a forbidden double spend-
/// an attempt to spend the same satoshis twice."
///
/// <https://developer.bitcoin.org/devguide/block_chain.html#introduction>
///
/// A _nullifier_ *MUST NOT* repeat either within a _transaction_, or across _transactions_ in a
/// _valid blockchain_ . *Sprout* and *Sapling* and *Orchard* _nulliers_ are considered disjoint,
/// even if they have the same bit pattern.
///
/// <https://zips.z.cash/protocol/protocol.pdf#nullifierset>
pub fn spend_conflicts(transaction: &Transaction) -> Result<(), TransactionError> {
    use crate::error::TransactionError::*;

    let transparent_outpoints = transaction.spent_outpoints().map(Cow::Owned);
    let sprout_nullifiers = transaction.sprout_nullifiers().map(Cow::Borrowed);
    let sapling_nullifiers = transaction.sapling_nullifiers().map(Cow::Borrowed);
    let orchard_nullifiers = transaction.orchard_nullifiers().map(Cow::Borrowed);

    check_for_duplicates(transparent_outpoints, DuplicateTransparentSpend)?;
    check_for_duplicates(sprout_nullifiers, DuplicateSproutNullifier)?;
    check_for_duplicates(sapling_nullifiers, DuplicateSaplingNullifier)?;
    check_for_duplicates(orchard_nullifiers, DuplicateOrchardNullifier)?;

    Ok(())
}

/// Check for duplicate items in a collection.
///
/// Each item should be wrapped by a [`Cow`] instance so that this helper function can properly
/// handle borrowed items and owned items.
///
/// If a duplicate is found, an error created by the `error_wrapper` is returned.
fn check_for_duplicates<'t, T>(
    items: impl IntoIterator<Item = Cow<'t, T>>,
    error_wrapper: impl FnOnce(T) -> TransactionError,
) -> Result<(), TransactionError>
where
    T: Clone + Eq + Hash + 't,
{
    let mut hash_set = HashSet::new();

    for item in items {
        if let Some(duplicate) = hash_set.replace(item) {
            return Err(error_wrapper(duplicate.into_owned()));
        }
    }

    Ok(())
}

/// Checks compatibility with [ZIP-212] shielded Sapling and Orchard coinbase output decryption
///
/// Pre-Heartwood: returns `Ok`.
/// Heartwood-onward: returns `Ok` if all Sapling or Orchard outputs, if any, decrypt successfully with
/// an all-zeroes outgoing viewing key. Returns `Err` otherwise.
///
/// This is used to validate coinbase transactions:
///
/// # Consensus
///
/// > [Heartwood onward] All Sapling and Orchard outputs in coinbase transactions MUST decrypt to a note
/// > plaintext, i.e. the procedure in § 4.19.3 ‘Decryption using a Full Viewing Key ( Sapling and Orchard )’ on p. 67
/// > does not return ⊥, using a sequence of 32 zero bytes as the outgoing viewing key. (This implies that before
/// > Canopy activation, Sapling outputs of a coinbase transaction MUST have note plaintext lead byte equal to
/// > 0x01.)
///
/// > [Canopy onward] Any Sapling or Orchard output of a coinbase transaction decrypted to a note plaintext
/// > according to the preceding rule MUST have note plaintext lead byte equal to 0x02. (This applies even during
/// > the "grace period" specified in [ZIP-212].)
///
/// <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
///
/// [ZIP-212]: https://zips.z.cash/zip-0212#consensus-rule-change-for-coinbase-transactions
///
/// TODO: Currently, a 0x01 lead byte is allowed in the "grace period" mentioned since we're
/// using `librustzcash` to implement this and it doesn't currently allow changing that behavior.
/// <https://github.com/ZcashFoundation/zebra/issues/3027>
pub fn coinbase_outputs_are_decryptable(
    transaction: &Transaction,
    network: Network,
    height: Height,
) -> Result<(), TransactionError> {
    // The consensus rule only applies to Heartwood onward.
    if height
        < NetworkUpgrade::Heartwood
            .activation_height(network)
            .expect("Heartwood height is known")
    {
        return Ok(());
    }

    if !zcash_note_encryption::decrypts_successfully(transaction, network, height) {
        return Err(TransactionError::CoinbaseOutputsNotDecryptable);
    }

    Ok(())
}

/// Returns `Ok(())` if the expiry height for the coinbase transaction is valid
/// according to specifications [7.1] and [ZIP-203].
///
/// [7.1]: https://zips.z.cash/protocol/protocol.pdf#txnencodingandconsensus
/// [ZIP-203]: https://zips.z.cash/zip-0203
pub fn coinbase_expiry_height(
    block_height: &Height,
    coinbase: &Transaction,
    network: Network,
) -> Result<(), TransactionError> {
    let expiry_height = coinbase.expiry_height();

    // TODO: replace `if let` with `expect` after NU5 mainnet activation
    if let Some(nu5_activation_height) = NetworkUpgrade::Nu5.activation_height(network) {
        // # Consensus
        //
        // > [NU5 onward] The nExpiryHeight field of a coinbase transaction
        // > MUST be equal to its block height.
        //
        // <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
        if *block_height >= nu5_activation_height {
            if expiry_height != Some(*block_height) {
                return Err(TransactionError::CoinbaseExpiryBlockHeight {
                    expiry_height,
                    block_height: *block_height,
                    transaction_hash: coinbase.hash(),
                });
            } else {
                return Ok(());
            }
        }
    }

    // # Consensus
    //
    // > [Overwinter to Canopy inclusive, pre-NU5] nExpiryHeight MUST be less than
    // > or equal to 499999999.
    //
    // <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
    validate_expiry_height_max(expiry_height, true, block_height, coinbase)
}

/// Returns `Ok(())` if the expiry height for a non coinbase transaction is
/// valid according to specifications [7.1] and [ZIP-203].
///
/// [7.1]: https://zips.z.cash/protocol/protocol.pdf#txnencodingandconsensus
/// [ZIP-203]: https://zips.z.cash/zip-0203
pub fn non_coinbase_expiry_height(
    block_height: &Height,
    transaction: &Transaction,
) -> Result<(), TransactionError> {
    if transaction.is_overwintered() {
        let expiry_height = transaction.expiry_height();

        // # Consensus
        //
        // > [Overwinter to Canopy inclusive, pre-NU5] nExpiryHeight MUST be
        // > less than or equal to 499999999.
        //
        // > [NU5 onward] nExpiryHeight MUST be less than or equal to 499999999
        // > for non-coinbase transactions.
        //
        // <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
        validate_expiry_height_max(expiry_height, false, block_height, transaction)?;

        // # Consensus
        //
        // > [Overwinter onward] If a transaction is not a coinbase transaction and its
        // > nExpiryHeight field is nonzero, then it MUST NOT be mined at a block
        // > height greater than its nExpiryHeight.
        //
        // <https://zips.z.cash/protocol/protocol.pdf#txnconsensus>
        validate_expiry_height_mined(expiry_height, block_height, transaction)?;
    }
    Ok(())
}

/// Checks that the expiry height of a transaction does not exceed the maximal
/// value.
///
/// Only the `expiry_height` parameter is used for the check. The
/// remaining parameters are used to give details about the error when the check
/// fails.
fn validate_expiry_height_max(
    expiry_height: Option<Height>,
    is_coinbase: bool,
    block_height: &Height,
    transaction: &Transaction,
) -> Result<(), TransactionError> {
    if let Some(expiry_height) = expiry_height {
        if expiry_height > Height::MAX_EXPIRY_HEIGHT {
            return Err(TransactionError::MaximumExpiryHeight {
                expiry_height,
                is_coinbase,
                block_height: *block_height,
                transaction_hash: transaction.hash(),
            })?;
        }
    }

    Ok(())
}

/// Checks that a transaction does not exceed its expiry height.
///
/// The `transaction` parameter is only used to give details about the error
/// when the check fails.
fn validate_expiry_height_mined(
    expiry_height: Option<Height>,
    block_height: &Height,
    transaction: &Transaction,
) -> Result<(), TransactionError> {
    if let Some(expiry_height) = expiry_height {
        if *block_height > expiry_height {
            return Err(TransactionError::ExpiredTransaction {
                expiry_height,
                block_height: *block_height,
                transaction_hash: transaction.hash(),
            })?;
        }
    }

    Ok(())
}
