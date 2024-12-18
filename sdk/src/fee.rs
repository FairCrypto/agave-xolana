//! Fee structures.

use logger::trace;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use lazy_static::lazy_static;
use crate::native_token::sol_to_lamports;
use solana_program::borsh1::try_from_slice_unchecked;
use solana_program::clock::{Epoch, Slot};
use solana_program::epoch_schedule::EpochSchedule;
use solana_program::instruction::{CompiledInstruction, InstructionError};
#[cfg(not(target_os = "solana"))]
use solana_program::message::SanitizedMessage;
use solana_program::pubkey::Pubkey;
use crate::{compute_budget, feature_set};
use crate::compute_budget::ComputeBudgetInstruction;
use crate::feature_set::{FEATURE_NAMES, full_inflation, FULL_INFLATION_FEATURE_PAIRS, include_loaded_accounts_data_size_in_fee_calculation, reduce_stake_warmup_cooldown};
use crate::transaction::{TransactionError};

pub const COMPUTE_UNIT_TO_US_RATIO: u64 = 30;
pub const SIGNATURE_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 24;
/// Number of compute units for one secp256k1 signature verification.
pub const SECP256K1_VERIFY_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 223;
/// Number of compute units for one ed25519 signature verification.
pub const ED25519_VERIFY_COST: u64 = COMPUTE_UNIT_TO_US_RATIO * 76;
pub const WRITE_LOCK_UNITS: u64 = COMPUTE_UNIT_TO_US_RATIO * 10;
pub const DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT: u32 = 200_000;
pub const MAX_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;
pub const HEAP_LENGTH: usize = 32 * 1024;
const MAX_HEAP_FRAME_BYTES: u32 = 256 * 1024;
pub const MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES: u32 = 64 * 1024 * 1024;
pub const DEFAULT_HEAP_COST: u64 = 8;
pub const INSTRUCTION_DATA_BYTES_COST: u64 = 140 /*bytes per us*/ / COMPUTE_UNIT_TO_US_RATIO;


/// Value used to indicate that a serialized account is not a duplicate
pub const NON_DUP_MARKER: u8 = u8::MAX;

lazy_static! {
    pub static ref BUILT_IN_INSTRUCTION_COSTS: HashMap<Pubkey, u64> = [
        (Pubkey::from_str("Stake11111111111111111111111111111111111111").unwrap(), 750u64),
        (Pubkey::from_str("Config1111111111111111111111111111111111111").unwrap(), 450u64),
        (Pubkey::from_str("Vote111111111111111111111111111111111111111").unwrap(), 2_100u64),
        (Pubkey::from_str("11111111111111111111111111111111").unwrap(), 150u64),
        (Pubkey::from_str("ComputeBudget111111111111111111111111111111").unwrap(), 150u64),
        (Pubkey::from_str("AddressLookupTab1e1111111111111111111111111").unwrap(), 750u64),
        (Pubkey::from_str("BPFLoaderUpgradeab1e11111111111111111111111").unwrap(), 2_370u64),
        (Pubkey::from_str("BPFLoader1111111111111111111111111111111111").unwrap(), 1_140u64),
        (Pubkey::from_str("BPFLoader2111111111111111111111111111111111").unwrap(), 570u64),
        (Pubkey::from_str("LoaderV411111111111111111111111111111111111").unwrap(), 2_000u64),
        // Note: These are precompile, run directly in bank during sanitizing;
        (Pubkey::from_str("KeccakSecp256k11111111111111111111111111111").unwrap(), 0u64),
        (Pubkey::from_str("Ed25519SigVerify111111111111111111111111111").unwrap(), 0u64)
    ]
    .iter()
    .cloned()
    .collect();
}

#[cfg_attr(feature = "frozen-abi", derive(AbiExample))]
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FeatureSet {
    pub active: HashMap<Pubkey, Slot>,
    pub inactive: HashSet<Pubkey>,
}
impl Default for FeatureSet {
    fn default() -> Self {
        // All features disabled
        Self {
            active: HashMap::new(),
            inactive: FEATURE_NAMES.keys().cloned().collect(),
        }
    }
}
impl FeatureSet {
    pub fn is_active(&self, feature_id: &Pubkey) -> bool {
        self.active.contains_key(feature_id)
    }

    pub fn activated_slot(&self, feature_id: &Pubkey) -> Option<Slot> {
        self.active.get(feature_id).copied()
    }

    /// List of enabled features that trigger full inflation
    pub fn full_inflation_features_enabled(&self) -> HashSet<Pubkey> {
        let mut hash_set = FULL_INFLATION_FEATURE_PAIRS
            .iter()
            .filter_map(|pair| {
                if self.is_active(&pair.vote_id) && self.is_active(&pair.enable_id) {
                    Some(pair.enable_id)
                } else {
                    None
                }
            })
            .collect::<HashSet<_>>();

        if self.is_active(&full_inflation::devnet_and_testnet::id()) {
            hash_set.insert(full_inflation::devnet_and_testnet::id());
        }
        hash_set
    }

    /// All features enabled, useful for testing
    pub fn all_enabled() -> Self {
        Self {
            active: FEATURE_NAMES.keys().cloned().map(|key| (key, 0)).collect(),
            inactive: HashSet::new(),
        }
    }

    /// Activate a feature
    pub fn activate(&mut self, feature_id: &Pubkey, slot: u64) {
        self.inactive.remove(feature_id);
        self.active.insert(*feature_id, slot);
    }

    /// Deactivate a feature
    pub fn deactivate(&mut self, feature_id: &Pubkey) {
        self.active.remove(feature_id);
        self.inactive.insert(*feature_id);
    }

    pub fn new_warmup_cooldown_rate_epoch(&self, epoch_schedule: &EpochSchedule) -> Option<Epoch> {
        self.activated_slot(&reduce_stake_warmup_cooldown::id())
            .map(|slot| epoch_schedule.get_epoch(slot))
    }
}

pub fn get_signature_cost_from_message(tx_cost: &mut UsageCostDetails, message: &SanitizedMessage) {
    // Get the signature details from the message
    let signatures_count_detail = message.get_signature_details();

    // Set the details to the tx_cost structure
    tx_cost.num_transaction_signatures = signatures_count_detail.num_transaction_signatures();
    tx_cost.num_secp256k1_instruction_signatures = signatures_count_detail.num_secp256k1_instruction_signatures();
    tx_cost.num_ed25519_instruction_signatures = signatures_count_detail.num_ed25519_instruction_signatures();

    // Calculate the signature cost based on the number of signatures
    tx_cost.signature_cost = signatures_count_detail
        .num_transaction_signatures()
        .saturating_mul(SIGNATURE_COST)
        .saturating_add(
            signatures_count_detail
                .num_secp256k1_instruction_signatures()
                .saturating_mul(SECP256K1_VERIFY_COST),
        )
        .saturating_add(
            signatures_count_detail
                .num_ed25519_instruction_signatures()
                .saturating_mul(ED25519_VERIFY_COST),
        );
}

/*
fn get_writable_accounts(message: &SanitizedMessage) -> Vec<Pubkey> {
    message
        .account_keys()
        .iter()
        .enumerate()
        .filter_map(|(i, k)| {
            if message.is_writable(i) {
                Some(*k)
            } else {
                None
            }
        })
        .collect()
}
*/

fn get_write_lock_cost(
    tx_cost: &mut UsageCostDetails,
    message: &SanitizedMessage,
    feature_set: &FeatureSet,
) {
    tx_cost.writable_accounts = vec![]; //get_writable_accounts(transaction);
    let num_write_locks =
        if feature_set.is_active(&feature_set::cost_model_requested_write_lock_cost::id()) {
            message.num_write_locks()
        } else {
            tx_cost.writable_accounts.len() as u64
        };
    tx_cost.write_lock_cost = WRITE_LOCK_UNITS.saturating_mul(num_write_locks);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ComputeBudgetLimits {
    pub updated_heap_bytes: u32,
    pub compute_unit_limit: u32,
    pub compute_unit_price: u64,
    pub loaded_accounts_bytes: u32,
}

fn sanitize_requested_heap_size(bytes: u32) -> bool {
    (u32::try_from(HEAP_LENGTH).unwrap()..=MAX_HEAP_FRAME_BYTES).contains(&bytes)
        && bytes % 1024 == 0
}
pub fn process_compute_budget_instructions<'a>(
    instructions: impl Iterator<Item = (&'a Pubkey, &'a CompiledInstruction)>,
) -> Result<ComputeBudgetLimits, TransactionError> {
    let mut num_non_compute_budget_instructions: u32 = 0;
    let mut updated_compute_unit_limit = None;
    let mut updated_compute_unit_price = None;
    let mut requested_heap_size = None;
    let mut updated_loaded_accounts_data_size_limit = None;

    for (i, (program_id, instruction)) in instructions.enumerate() {
        if compute_budget::check_id(program_id) {
            let invalid_instruction_data_error = TransactionError::InstructionError(
                i as u8,
                InstructionError::InvalidInstructionData,
            );
            let duplicate_instruction_error = TransactionError::DuplicateInstruction(i as u8);

            match try_from_slice_unchecked(&instruction.data) {
                Ok(ComputeBudgetInstruction::RequestHeapFrame(bytes)) => {
                    if requested_heap_size.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    if sanitize_requested_heap_size(bytes) {
                        requested_heap_size = Some(bytes);
                    } else {
                        return Err(invalid_instruction_data_error);
                    }
                }
                Ok(ComputeBudgetInstruction::SetComputeUnitLimit(compute_unit_limit)) => {
                    if updated_compute_unit_limit.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    updated_compute_unit_limit = Some(compute_unit_limit);
                }
                Ok(ComputeBudgetInstruction::SetComputeUnitPrice(micro_lamports)) => {
                    if updated_compute_unit_price.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    updated_compute_unit_price = Some(micro_lamports);
                }
                Ok(ComputeBudgetInstruction::SetLoadedAccountsDataSizeLimit(bytes)) => {
                    if updated_loaded_accounts_data_size_limit.is_some() {
                        return Err(duplicate_instruction_error);
                    }
                    updated_loaded_accounts_data_size_limit = Some(bytes);
                }
                _ => return Err(invalid_instruction_data_error),
            }
        } else {
            // only include non-request instructions in default max calc
            num_non_compute_budget_instructions =
                num_non_compute_budget_instructions.saturating_add(1);
        }
    }

    // sanitize limits
    let updated_heap_bytes = requested_heap_size
        .unwrap_or(u32::try_from(HEAP_LENGTH).unwrap()) // loader's default heap_size
        .min(MAX_HEAP_FRAME_BYTES);

    let compute_unit_limit = updated_compute_unit_limit
        .unwrap_or_else(|| {
            num_non_compute_budget_instructions
                .saturating_mul(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT)
        })
        .min(MAX_COMPUTE_UNIT_LIMIT);

    let compute_unit_price = updated_compute_unit_price.unwrap_or(0);

    let loaded_accounts_bytes = updated_loaded_accounts_data_size_limit
        .unwrap_or(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES)
        .min(MAX_LOADED_ACCOUNTS_DATA_SIZE_BYTES);

    Ok(ComputeBudgetLimits {
        updated_heap_bytes,
        compute_unit_limit,
        compute_unit_price,
        loaded_accounts_bytes,
    })
}

fn get_compute_unit_price_from_message(
    tx_cost: &mut UsageCostDetails,
    message: &SanitizedMessage,
) {
    // Iterate through instructions and search for ComputeBudgetInstruction::SetComputeUnitPrice
    for (program_id, instruction) in message.program_instructions_iter() {
        if compute_budget::check_id(program_id) {
            if let Ok(ComputeBudgetInstruction::SetComputeUnitPrice(price)) =
                try_from_slice_unchecked(&instruction.data)
            {
                // Set the compute unit price in tx_cost
                tx_cost.compute_unit_price = price;
            }
        }
    }
}


fn get_transaction_cost(
    tx_cost: &mut UsageCostDetails,
    message: &SanitizedMessage,
    feature_set: &FeatureSet,
) {
    let mut builtin_costs = 0u64;
    let mut bpf_costs = 0u64;
    let mut loaded_accounts_data_size_cost = 0u64;
    let mut data_bytes_len_total = 0u64;
    let mut compute_unit_limit_is_set = false;

    for (program_id, instruction) in message.program_instructions_iter() {
        // to keep the same behavior, look for builtin first
        if let Some(builtin_cost) = BUILT_IN_INSTRUCTION_COSTS.get(program_id) {
            builtin_costs = builtin_costs.saturating_add(*builtin_cost);
        } else {
            bpf_costs = bpf_costs
                .saturating_add(u64::from(DEFAULT_INSTRUCTION_COMPUTE_UNIT_LIMIT))
                .min(u64::from(MAX_COMPUTE_UNIT_LIMIT));
        }
        data_bytes_len_total =
            data_bytes_len_total.saturating_add(instruction.data.len() as u64);

        if compute_budget::check_id(program_id) {
            if let Ok(ComputeBudgetInstruction::SetComputeUnitLimit(_)) =
                try_from_slice_unchecked(&instruction.data)
            {
                compute_unit_limit_is_set = true;
            }
        }
    }

    // calculate bpf cost based on compute budget instructions

    // if failed to process compute_budget instructions, the transaction will not be executed
    // by `bank`, therefore it should be considered as no execution cost by cost model.
    match process_compute_budget_instructions(message.program_instructions_iter())
    {
        Ok(compute_budget_limits) => {
            // if tx contained user-space instructions and a more accurate estimate available correct it,
            // where "user-space instructions" must be specifically checked by
            // 'compute_unit_limit_is_set' flag, because compute_budget does not distinguish
            // builtin and bpf instructions when calculating default compute-unit-limit. (see
            // compute_budget.rs test `test_process_mixed_instructions_without_compute_budget`)
            if bpf_costs > 0 && compute_unit_limit_is_set {
                bpf_costs = u64::from(compute_budget_limits.compute_unit_limit);
            }

            if feature_set
                .is_active(&include_loaded_accounts_data_size_in_fee_calculation::id())
            {
                loaded_accounts_data_size_cost = FeeStructure::calculate_memory_usage_cost(
                    usize::try_from(compute_budget_limits.loaded_accounts_bytes).unwrap(),
                    DEFAULT_HEAP_COST,
                )
            }
        }
        Err(_) => {
            builtin_costs = 0;
            bpf_costs = 0;
        }
    }

    tx_cost.builtins_execution_cost = builtin_costs;
    tx_cost.bpf_execution_cost = bpf_costs;
    tx_cost.loaded_accounts_data_size_cost = loaded_accounts_data_size_cost;
    tx_cost.data_bytes_cost = data_bytes_len_total / INSTRUCTION_DATA_BYTES_COST;
}

const MAX_WRITABLE_ACCOUNTS: usize = 256;

// costs are stored in number of 'compute unit's
#[derive(Debug)]
pub struct UsageCostDetails {
    pub writable_accounts: Vec<Pubkey>,
    pub signature_cost: u64,
    pub write_lock_cost: u64,
    pub data_bytes_cost: u64,
    pub builtins_execution_cost: u64,
    pub bpf_execution_cost: u64,
    pub loaded_accounts_data_size_cost: u64,
    pub account_data_size: u64,
    pub num_transaction_signatures: u64,
    pub num_secp256k1_instruction_signatures: u64,
    pub num_ed25519_instruction_signatures: u64,
    pub compute_unit_price: u64,
}

impl Default for UsageCostDetails {
    fn default() -> Self {
        Self {
            writable_accounts: Vec::with_capacity(MAX_WRITABLE_ACCOUNTS),
            signature_cost: 0u64,
            write_lock_cost: 0u64,
            data_bytes_cost: 0u64,
            builtins_execution_cost: 0u64,
            bpf_execution_cost: 0u64,
            loaded_accounts_data_size_cost: 0u64,
            account_data_size: 0u64,
            num_transaction_signatures: 0u64,
            num_secp256k1_instruction_signatures: 0u64,
            num_ed25519_instruction_signatures: 0u64,
            compute_unit_price: 0u64,
        }
    }
}

#[cfg(test)]
impl PartialEq for UsageCostDetails {
    fn eq(&self, other: &Self) -> bool {
        fn to_hash_set(v: &[Pubkey]) -> std::collections::HashSet<&Pubkey> {
            v.iter().collect()
        }

        self.signature_cost == other.signature_cost
            && self.write_lock_cost == other.write_lock_cost
            && self.data_bytes_cost == other.data_bytes_cost
            && self.builtins_execution_cost == other.builtins_execution_cost
            && self.bpf_execution_cost == other.bpf_execution_cost
            && self.loaded_accounts_data_size_cost == other.loaded_accounts_data_size_cost
            && self.account_data_size == other.account_data_size
            && self.num_transaction_signatures == other.num_transaction_signatures
            && self.num_secp256k1_instruction_signatures
            == other.num_secp256k1_instruction_signatures
            && self.num_ed25519_instruction_signatures == other.num_ed25519_instruction_signatures
            && to_hash_set(&self.writable_accounts) == to_hash_set(&other.writable_accounts)
    }
}


/// A fee and its associated compute unit limit
#[derive(Debug, Default, Clone, Eq, PartialEq)]
pub struct FeeBin {
    /// maximum compute units for which this fee will be charged
    pub limit: u64,
    /// fee in lamports
    pub fee: u64,
}

pub struct FeeBudgetLimits {
    pub loaded_accounts_data_size_limit: usize,
    pub heap_cost: u64,
    pub compute_unit_limit: u64,
    pub prioritization_fee: u64,
}

/// Information used to calculate fees
#[derive(Debug, Clone, Eq, PartialEq)]
pub struct FeeStructure {
    /// lamports per signature
    pub lamports_per_signature: u64,
    /// lamports_per_write_lock
    pub lamports_per_write_lock: u64,
    /// Compute unit fee bins
    pub compute_fee_bins: Vec<FeeBin>,
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Deserialize, Serialize)]
pub struct FeeDetails {
    transaction_fee: u64,
    prioritization_fee: u64,
    remove_rounding_in_fee_calculation: bool,
}

impl FeeDetails {
    #[cfg(feature = "dev-context-only-utils")]
    pub fn new_for_tests(
        transaction_fee: u64,
        prioritization_fee: u64,
        remove_rounding_in_fee_calculation: bool,
    ) -> Self {
        Self {
            transaction_fee,
            prioritization_fee,
            remove_rounding_in_fee_calculation,
        }
    }

    pub fn total_fee(&self) -> u64 {
        self.transaction_fee
    }

    pub fn accumulate(&mut self, fee_details: &FeeDetails) {
        self.transaction_fee = self
            .transaction_fee
            .saturating_add(fee_details.transaction_fee);
        self.prioritization_fee = self
            .prioritization_fee
            .saturating_add(fee_details.prioritization_fee)
    }

    pub fn transaction_fee(&self) -> u64 {
        self.transaction_fee
    }

    pub fn prioritization_fee(&self) -> u64 {
        self.prioritization_fee
    }
}

pub const ACCOUNT_DATA_COST_PAGE_SIZE: u64 = 32_u64.saturating_mul(1024);

impl FeeStructure {
    pub fn new(
        sol_per_signature: f64,
        sol_per_write_lock: f64,
        compute_fee_bins: Vec<(u64, f64)>,
    ) -> Self {
        let compute_fee_bins = compute_fee_bins
            .iter()
            .map(|(limit, sol)| FeeBin {
                limit: *limit,
                fee: sol_to_lamports(*sol),
            })
            .collect::<Vec<_>>();
        FeeStructure {
            lamports_per_signature: sol_to_lamports(sol_per_signature),
            lamports_per_write_lock: sol_to_lamports(sol_per_write_lock),
            compute_fee_bins,
        }
    }

    pub fn get_max_fee(&self, num_signatures: u64, num_write_locks: u64) -> u64 {
        num_signatures
            .saturating_mul(self.lamports_per_signature)
            .saturating_add(num_write_locks.saturating_mul(self.lamports_per_write_lock))
            .saturating_add(
                self.compute_fee_bins
                    .last()
                    .map(|bin| bin.fee)
                    .unwrap_or_default(),
            )
    }

    pub fn calculate_memory_usage_cost(
        loaded_accounts_data_size_limit: usize,
        heap_cost: u64,
    ) -> u64 {
        (loaded_accounts_data_size_limit as u64)
            .saturating_add(ACCOUNT_DATA_COST_PAGE_SIZE.saturating_sub(1))
            .saturating_div(ACCOUNT_DATA_COST_PAGE_SIZE)
            .saturating_mul(heap_cost)
    }

    /// Calculate fee for `SanitizedMessage`
    #[cfg(not(target_os = "solana"))]
    pub fn calculate_fee(
        &self,
        message: &SanitizedMessage,
        lamports_per_signature: u64,
        budget_limits: &FeeBudgetLimits,
        include_loaded_account_data_size_in_fee: bool,
        remove_rounding_in_fee_calculation: bool,
    ) -> u64 {
        self.calculate_fee_details(
            message,
            lamports_per_signature,
            budget_limits,
            include_loaded_account_data_size_in_fee,
            remove_rounding_in_fee_calculation,
        )
        .total_fee()
    }

    /// Calculate fee details for `SanitizedMessage`
    #[cfg(not(target_os = "solana"))]
    pub fn calculate_fee_details(
        &self,
        message: &SanitizedMessage,
        lamports_per_signature: u64,
        budget_limits: &FeeBudgetLimits,
        _include_loaded_account_data_size_in_fee: bool,
        remove_rounding_in_fee_calculation: bool,
    ) -> FeeDetails {
        trace!("Calculating fee with lamports_per_signature: {} and self.lamports_per_signature {} ", lamports_per_signature, self.lamports_per_signature);
        trace!("Dump message: {:?}", message);
        trace!("Dump keys: {:?}", message.account_keys());

        let vote_program_id = &solana_sdk::vote::program::id();
        let contains_vote_program = message.account_keys().iter().any(|key| key == vote_program_id);

        let mut tx_cost = UsageCostDetails::default();
        get_signature_cost_from_message(&mut tx_cost, &message);
        get_write_lock_cost(&mut tx_cost, message, &FeatureSet::default());
        get_transaction_cost(&mut tx_cost, message, &FeatureSet::default());
        get_compute_unit_price_from_message(&mut tx_cost, &message);

        let derived_cu = tx_cost.builtins_execution_cost.saturating_add(tx_cost.bpf_execution_cost);

        let adjusted_cu_price = if derived_cu < 1000 && tx_cost.compute_unit_price < 1_000_000 {
            1_000_000
        } else {
            tx_cost.compute_unit_price
        };

        let mut total_fee = derived_cu
            .saturating_mul(10)
            .saturating_add(derived_cu.saturating_mul(adjusted_cu_price as u64) / 1_000_000);

        // If the message contains the vote program, set the total fee to 0
        if contains_vote_program {
            trace!("Vote program detected, setting total_fee to 0");
            total_fee = 0;
        } else {
            trace!("Calculated total_fee: {} with compute units: {} compute unit price {}", total_fee, derived_cu, tx_cost.compute_unit_price);
        }

        FeeDetails {
            transaction_fee: total_fee,
            prioritization_fee: budget_limits.prioritization_fee,
            remove_rounding_in_fee_calculation,
        }
    }
}

impl Default for FeeStructure {
    fn default() -> Self {
        Self::new(0.000000005, 0.0, vec![(1_400_000, 0.0)])
    }
}

#[cfg(all(RUSTC_WITH_SPECIALIZATION, feature = "frozen-abi"))]
impl ::solana_frozen_abi::abi_example::AbiExample for FeeStructure {
    fn example() -> Self {
        FeeStructure::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_memory_usage_cost() {
        let heap_cost = 99;
        const K: usize = 1024;

        // accounts data size are priced in block of 32K, ...

        // ... requesting less than 32K should still be charged as one block
        assert_eq!(
            heap_cost,
            FeeStructure::calculate_memory_usage_cost(31 * K, heap_cost)
        );

        // ... requesting exact 32K should be charged as one block
        assert_eq!(
            heap_cost,
            FeeStructure::calculate_memory_usage_cost(32 * K, heap_cost)
        );

        // ... requesting slightly above 32K should be charged as 2 block
        assert_eq!(
            heap_cost * 2,
            FeeStructure::calculate_memory_usage_cost(33 * K, heap_cost)
        );

        // ... requesting exact 64K should be charged as 2 block
        assert_eq!(
            heap_cost * 2,
            FeeStructure::calculate_memory_usage_cost(64 * K, heap_cost)
        );
    }

    #[test]
    fn test_total_fee_rounding() {
        // round large `f64` can lost precision, see feature gate:
        // "Removing unwanted rounding in fee calculation #34982"

        let transaction_fee = u64::MAX - 11;
        let prioritization_fee = 1;
        let expected_large_fee = u64::MAX - 10;

        let details_with_rounding = FeeDetails {
            transaction_fee,
            prioritization_fee,
            remove_rounding_in_fee_calculation: false,
        };
        let details_without_rounding = FeeDetails {
            transaction_fee,
            prioritization_fee,
            remove_rounding_in_fee_calculation: true,
        };

        assert_eq!(details_without_rounding.total_fee(), expected_large_fee);
        assert_ne!(details_with_rounding.total_fee(), expected_large_fee);
    }
}
