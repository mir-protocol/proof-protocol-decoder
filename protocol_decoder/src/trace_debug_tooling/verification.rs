// TODO: Replace all `unwraps()` with actual errors...

use core::fmt;
use std::{
    collections::{HashMap, HashSet},
    fmt::{Display, LowerHex},
};

use eth_trie_utils::partial_trie::HashedPartialTrie;
use ethereum_types::{Address, H256};
use plonky2_evm::generation::mpt::AccountRlp;
use thiserror::Error;

use crate::{
    compact::compact_prestate_processing::PartialTriePreImages,
    processed_block_trace::{
        process_block_trace_trie_pre_images, ProcessedBlockTrace, ProcessedTxnInfo, ProcessingMeta,
    },
    trace_protocol::{BlockTrace, TxnInfo},
    types::{
        CodeHash, CodeHashResolveFunc, HashedAccountAddr, HashedAccountAddrNibbles,
        HashedStorageAddr, HashedStorageAddrNibbles, PartialTrieState, StorageAddr, TrieRootHash,
        TxnProofGenIR, EMPTY_CODE_HASH, EMPTY_TRIE_HASH,
    },
    utils::{get_leaf_vals_from_trie, get_leaf_vals_from_trie_and_decode, hash},
};

// All account storage roots in accounts exist in the storage trie after each
// txn. All account code hashes have a matching entry for every txn.
// All pre-image leafs are accessed by the traces.
// All final storage slots mentioned in trace match result from rpc call to full
// node. All account entry fields match call to rpc call to full node.
// Check that all roots match rpc call to full node.

pub(crate) type TraceVerificationResult<T> = Result<T, TraceVerificationErrors>;

#[derive(Debug)]
pub(crate) struct GeneratedProofAndDebugInfo {
    ir: Vec<TxnProofGenIR>,
    final_tries_after_each_txn: Vec<PartialTrieState>,
    processed_trace: ProcessedBlockTrace,
}

#[derive(Debug, Error)]
pub struct TraceVerificationErrors {
    errs: Vec<TraceVerificationError>,
}

impl fmt::Display for TraceVerificationErrors {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "The following verification errors occurred:")?;

        for err in self.errs.iter() {
            writeln!(f, "{}", err)?;
        }

        Ok(())
    }
}

#[derive(Clone, Debug, Error)]
pub enum TraceVerificationError {
    #[error("No storage trie supplied for storage root {2:x} which is referenced by account {0} (hashed: {1:x})")]
    MissingStorageTrieForAccount(
        HashLookupAttempt<Address>,
        HashedAccountAddrNibbles,
        TrieRootHash,
    ),

    #[error("No contract bytecode supplied for code hash {2:x} which is referenced by account {0} (hashed: {1:x})")]
    MissingContractCodeForAccount(
        HashLookupAttempt<Address>,
        HashedAccountAddrNibbles,
        CodeHash,
    ),
}

/// Wrapper around an `Option` just to make errors a bit more readable.
#[derive(Clone, Debug)]
struct HashLookupAttempt<T: LowerHex>(Option<T>);

impl<T: LowerHex> Display for HashLookupAttempt<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.0 {
            Some(v) => write!(f, "{:x}", v),
            None => write!(f, "Unknown"),
        }
    }
}

impl<T: LowerHex> From<Option<T>> for HashLookupAttempt<T> {
    fn from(v: Option<T>) -> Self {
        Self(v)
    }
}

#[derive(Debug)]
struct ReverseHashMapping {
    hashed_addr_to_addr: HashMap<HashedAccountAddrNibbles, HashLookupAttempt<Address>>,
    hashed_slot_to_slot: HashMap<HashedStorageAddrNibbles, HashLookupAttempt<StorageAddr>>,
}

pub(crate) fn verify_proof_gen_ir<F: CodeHashResolveFunc>(
    b_trace: &BlockTrace,
    p_meta: &ProcessingMeta<F>,
) -> TraceVerificationResult<()> {
    let mut err_buf = Vec::default();
    let pre_image = process_block_trace_trie_pre_images(b_trace.trie_pre_images.clone(), false);
    let code_supplied_by_pre_image = pre_image.extra_code_hash_mappings.unwrap_or_default();

    let reverse_hash_mapping =
        create_addr_to_hashed_addr_mapping(&pre_image.tries, &b_trace.txn_info);

    let proced_b_trace = b_trace.clone().into_processed_block_trace(p_meta, false);

    verify_all_referenced_code_exists_in_code_mapping(
        &pre_image.tries.state,
        &proced_b_trace.txn_info,
        &code_supplied_by_pre_image,
        &reverse_hash_mapping,
        &mut err_buf,
    );

    verify_all_prestate_storage_entries_point_to_existing_tries(
        &proced_b_trace.tries,
        &proced_b_trace.txn_info,
        &reverse_hash_mapping,
        &mut err_buf,
    );

    match err_buf.is_empty() {
        false => Err(TraceVerificationErrors { errs: err_buf }),
        true => Ok(()),
    }
}

fn verify_all_prestate_storage_entries_point_to_existing_tries(
    pre_image: &PartialTriePreImages,
    traces: &[ProcessedTxnInfo],
    reverse_hash_mapping: &ReverseHashMapping,
    err_buf: &mut Vec<TraceVerificationError>,
) {
    let contract_storage_accessed = get_all_storage_tries_that_are_accessed(traces);

    for (h_addr, acc) in get_leaf_vals_from_trie_and_decode::<AccountRlp>(&pre_image.state) {
        if acc.storage_root != EMPTY_TRIE_HASH
            && contract_storage_accessed.contains(&acc.storage_root)
            && !pre_image.storage.contains_key(&acc.storage_root)
        {
            let addr_lookup_attempt = reverse_hash_mapping.hashed_addr_to_addr[&h_addr].clone();

            err_buf.push(TraceVerificationError::MissingStorageTrieForAccount(
                addr_lookup_attempt,
                h_addr,
                acc.storage_root,
            ));
        }
    }
}

fn get_all_storage_tries_that_are_accessed(
    traces: &[ProcessedTxnInfo],
) -> HashSet<HashedAccountAddr> {
    traces
        .iter()
        .flat_map(|t| {
            t.nodes_used_by_txn
                .storage_accesses
                .iter()
                .map(|(h_addr, _)| H256::from_slice(&h_addr.bytes_be()))
        })
        .collect()
}

fn verify_all_account_storage_roots_exist_in_account_partial_trie() {
    todo!();
}

fn verify_all_referenced_code_exists_in_code_mapping(
    pre_image_state: &HashedPartialTrie,
    traces: &[ProcessedTxnInfo],
    code_supplied_by_pre_image: &HashMap<CodeHash, Vec<u8>>,
    reverse_hash_mapping: &ReverseHashMapping,
    err_buf: &mut Vec<TraceVerificationError>,
) {
    // TODO: For now, we are going to make the assumption that all byte code is
    // provided in the pre-state trie. This assumption may change in the future, and
    // if it does, we should either remove this check or put it behind a config
    // flag.

    let all_code_hashes_accessed = get_all_contract_code_hashes_that_are_accessed(traces);

    for (h_addr, acc) in get_leaf_vals_from_trie_and_decode::<AccountRlp>(pre_image_state) {
        if acc.code_hash != EMPTY_CODE_HASH
            && all_code_hashes_accessed.contains(&acc.code_hash)
            && !code_supplied_by_pre_image.contains_key(&acc.code_hash)
        {
            let addr_lookup_attempt = &reverse_hash_mapping.hashed_addr_to_addr[&h_addr];

            err_buf.push(TraceVerificationError::MissingContractCodeForAccount(
                addr_lookup_attempt.clone(),
                h_addr,
                acc.code_hash,
            ));
        }
    }
}

fn get_all_contract_code_hashes_that_are_accessed(
    traces: &[ProcessedTxnInfo],
) -> HashSet<CodeHash> {
    traces
        .iter()
        .flat_map(|t| t.contract_code_accessed.keys().cloned())
        .collect()
}

fn verify_all_pre_image_nodes_are_accessed_throughout_the_block() {
    todo!();
}

fn verify_all_final_block_storage_slots_match_full_node() {
    todo!();
}

fn verify_all_account_entry_nodes_match_full_node() {
    todo!();
}

fn verify_all_final_trie_roots_match_full_node() {
    todo!();
}

fn create_addr_to_hashed_addr_mapping(
    pre_state: &PartialTriePreImages,
    traces: &[TxnInfo],
) -> ReverseHashMapping {
    let trace_addr_to_h_addr: HashMap<_, _> = traces
        .iter()
        .flat_map(|txn_info| {
            txn_info
                .traces
                .keys()
                .map(|addr| (hash(addr.as_bytes()), *addr))
        })
        .collect();

    let trace_slot_to_h_slot: HashMap<_, _> = traces
        .iter()
        .flat_map(|txn_info| {
            txn_info.traces.iter().flat_map(|(_, trace)| {
                let all_account_slots = trace
                    .storage_read
                    .iter()
                    .flat_map(|x| x.iter())
                    .chain(trace.storage_written.iter().flat_map(|x| x.keys()));
                all_account_slots.map(|slot| (hash(slot.as_bytes()), *slot))
            })
        })
        .collect();

    let hashed_addr_to_addr = get_leaf_vals_from_trie(&pre_state.state)
        .map(|(h_addr_nibs, _)| {
            let h_addr = HashedAccountAddr::from_slice(&h_addr_nibs.bytes_be());
            let addr_lookup_res = trace_addr_to_h_addr.get(&h_addr).cloned().into();

            (h_addr_nibs, addr_lookup_res)
        })
        .collect();

    let hashed_slot_to_slot = pre_state
        .storage
        .iter()
        .flat_map(|(_, s_trie)| {
            get_leaf_vals_from_trie(s_trie).map(|(h_slot_nibs, _)| {
                let h_slot = HashedStorageAddr::from_slice(&h_slot_nibs.bytes_be());
                let slot_lookup_res = trace_slot_to_h_slot.get(&h_slot).cloned().into();

                (h_slot_nibs, slot_lookup_res)
            })
        })
        .collect();

    ReverseHashMapping {
        hashed_addr_to_addr,
        hashed_slot_to_slot,
    }
}
