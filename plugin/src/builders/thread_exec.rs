use std::sync::Arc;

use anchor_lang::{InstructionData, ToAccountMetas};
use log::info;
use sablier_network_program::state::Worker;
use sablier_thread_program::state::{Trigger, VersionedThread};
use sablier_utils::{pyth::get_oracle_key, thread::PAYER_PUBKEY};
use solana_account_decoder::UiAccountEncoding;
use solana_client::{
    nonblocking::rpc_client::RpcClient,
    rpc_config::{RpcSimulateTransactionAccountsConfig, RpcSimulateTransactionConfig},
    rpc_custom_error::JSON_RPC_SERVER_ERROR_MIN_CONTEXT_SLOT_NOT_REACHED,
};
use solana_sdk::{
    account::Account,
    commitment_config::CommitmentConfig,
    compute_budget::ComputeBudgetInstruction,
    instruction::{AccountMeta, Instruction},
    pubkey::Pubkey,
    signature::Keypair,
    signer::Signer,
    transaction::Transaction,
};

use crate::error::PluginError;

/// Max byte size of a serialized transaction.
static TRANSACTION_MESSAGE_SIZE_LIMIT: usize = 1_232;

/// Max compute units that may be used by transaction.
static TRANSACTION_COMPUTE_UNIT_LIMIT: u32 = 1_400_000;

/// The buffer amount to add to transactions' compute units in case on-chain PDA derivations take more CUs than used in simulation.
static TRANSACTION_COMPUTE_UNIT_BUFFER: u32 = 1000;

pub async fn build_thread_exec_tx(
    client: Arc<RpcClient>,
    payer: &Keypair,
    slot: u64,
    thread: VersionedThread,
    thread_pubkey: Pubkey,
    worker_id: u64,
) -> Result<Option<Transaction>, PluginError> {
    // Grab the thread and relevant data.
    let now = std::time::Instant::now();
    let blockhash = client.get_latest_blockhash().await?;
    let signatory_pubkey = payer.pubkey();
    let worker_pubkey = Worker::pubkey(worker_id);

    // Build the first instruction of the transaction.
    let first_instruction = if thread.next_instruction().is_some() {
        build_exec_ix(
            thread.clone(),
            thread_pubkey,
            signatory_pubkey,
            worker_pubkey,
        )
    } else {
        build_kickoff_ix(
            thread.clone(),
            thread_pubkey,
            signatory_pubkey,
            worker_pubkey,
        )
    };

    // Simulate the transaction and pack as many instructions as possible until we hit mem/cpu limits.
    // TODO Migrate to versioned transactions.
    let mut ixs: Vec<Instruction> = vec![
        ComputeBudgetInstruction::set_compute_unit_limit(TRANSACTION_COMPUTE_UNIT_LIMIT),
        first_instruction,
    ];
    let mut successful_ixs: Vec<Instruction> = vec![];
    let mut units_consumed: Option<u64> = None;
    loop {
        let mut sim_tx = Transaction::new_with_payer(&ixs, Some(&signatory_pubkey));
        sim_tx.sign(&[payer], blockhash);

        // Exit early if the transaction exceeds the size limit.
        if sim_tx.message_data().len() > TRANSACTION_MESSAGE_SIZE_LIMIT {
            info!("The transaction is too big to be send in one shot.");
            break;
        }

        // Run the simulation.
        match client
            .simulate_transaction_with_config(
                &sim_tx,
                RpcSimulateTransactionConfig {
                    sig_verify: false,
                    replace_recent_blockhash: true,
                    commitment: Some(CommitmentConfig::processed()),
                    accounts: Some(RpcSimulateTransactionAccountsConfig {
                        encoding: Some(UiAccountEncoding::Base64Zstd),
                        addresses: vec![thread_pubkey.to_string()],
                    }),
                    min_context_slot: Some(slot),
                    ..RpcSimulateTransactionConfig::default()
                },
            )
            .await
        {
            // If there was a simulation error, stop packing and exit now.
            Err(err) => {
                if let solana_client::client_error::ClientErrorKind::RpcError(
                    solana_client::rpc_request::RpcError::RpcResponseError { code, .. },
                ) = err.kind
                {
                    if code == JSON_RPC_SERVER_ERROR_MIN_CONTEXT_SLOT_NOT_REACHED {
                        return Err(PluginError::MinContextSlotNotReached);
                    }
                }
                break;
            }

            // If the simulation was successful, pack the ix into the tx.
            Ok(response) => {
                if response.value.err.is_some() {
                    if successful_ixs.is_empty() {
                        info!(
                            "slot: {} thread: {} simulation_error: \"{}\" logs: {:?}",
                            slot,
                            thread_pubkey,
                            response.value.err.unwrap(),
                            response.value.logs.unwrap_or(vec![]),
                        );
                    }
                    break;
                }

                // Update flag tracking if at least one instruction succeed.
                successful_ixs = ixs.clone();

                // Record the compute units consumed by the simulation.
                if response.value.units_consumed.is_some() {
                    units_consumed = response.value.units_consumed;
                }

                // Parse the resulting thread account for the next instruction to simulate.
                if let Some(ui_accounts) = response.value.accounts {
                    if let Some(Some(ui_account)) = ui_accounts.first() {
                        if let Some(account) = ui_account.decode::<Account>() {
                            if let Ok(sim_thread) = VersionedThread::try_from(account.data) {
                                if sim_thread.next_instruction().is_some() {
                                    if let Some(exec_context) = sim_thread.exec_context() {
                                        if exec_context
                                            .execs_since_slot
                                            .lt(&sim_thread.rate_limit())
                                        {
                                            ixs.push(build_exec_ix(
                                                sim_thread,
                                                thread_pubkey,
                                                signatory_pubkey,
                                                worker_pubkey,
                                            ));
                                        } else {
                                            // Exit early if the thread has reached its rate limit.
                                            break;
                                        }
                                    }
                                } else {
                                    break;
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // If there were no successful instructions, then exit early. There is nothing to do.
    // Alternatively, exit early if only the kickoff instruction (and no execs) succeeded.
    if successful_ixs.is_empty() {
        return Ok(None);
    }

    // Set the transaction's compute unit limit to be exactly the amount that was used in simulation.
    if let Some(units_consumed) = units_consumed {
        let units_committed = std::cmp::min(
            (units_consumed as u32) + TRANSACTION_COMPUTE_UNIT_BUFFER,
            TRANSACTION_COMPUTE_UNIT_LIMIT,
        );
        _ = std::mem::replace(
            &mut successful_ixs[0],
            ComputeBudgetInstruction::set_compute_unit_limit(units_committed),
        );
    }

    // Build and return the signed transaction.
    let mut tx = Transaction::new_with_payer(&successful_ixs, Some(&signatory_pubkey));
    tx.sign(&[payer], blockhash);
    info!(
        "slot: {:?} thread: {:?} sim_duration: {:?} instruction_count: {:?} compute_units: {:?} tx_sig: {:?}",
        slot,
        thread_pubkey,
        now.elapsed(),
        successful_ixs.len(),
        units_consumed,
        tx.signatures[0]
    );
    Ok(Some(tx))
}

fn build_kickoff_ix(
    thread: VersionedThread,
    thread_pubkey: Pubkey,
    signatory_pubkey: Pubkey,
    worker_pubkey: Pubkey,
) -> Instruction {
    // Build the instruction.
    let mut kickoff_ix = match thread {
        VersionedThread::V1(_) => Instruction {
            program_id: sablier_thread_program::ID,
            accounts: sablier_thread_program::accounts::ThreadKickoff {
                signatory: signatory_pubkey,
                thread: thread_pubkey,
                worker: worker_pubkey,
            }
            .to_account_metas(Some(false)),
            data: sablier_thread_program::instruction::ThreadKickoff {}.data(),
        },
    };

    // If the thread's trigger is account-based, inject the triggering account.
    match thread.trigger() {
        Trigger::Account {
            address,
            offset: _,
            size: _,
        } => kickoff_ix.accounts.push(AccountMeta {
            pubkey: address,
            is_signer: false,
            is_writable: false,
        }),
        Trigger::Pyth {
            feed_id,
            equality: _,
            limit: _,
        } => {
            let pubkey = get_oracle_key(0, feed_id);
            kickoff_ix.accounts.push(AccountMeta {
                pubkey,
                is_signer: false,
                is_writable: false,
            })
        }
        _ => {}
    }

    kickoff_ix
}

fn build_exec_ix(
    thread: VersionedThread,
    thread_pubkey: Pubkey,
    signatory_pubkey: Pubkey,
    worker_pubkey: Pubkey,
) -> Instruction {
    // Build the instruction.
    let mut exec_ix = match thread {
        VersionedThread::V1(_) => Instruction {
            program_id: sablier_thread_program::ID,
            accounts: sablier_thread_program::accounts::ThreadExec {
                fee: sablier_network_program::state::Fee::pubkey(worker_pubkey),
                pool: sablier_network_program::state::Pool::pubkey(0),
                signatory: signatory_pubkey,
                thread: thread_pubkey,
                worker: worker_pubkey,
            }
            .to_account_metas(Some(true)),
            data: sablier_thread_program::instruction::ThreadExec {}.data(),
        },
    };

    if let Some(next_instruction) = thread.next_instruction() {
        // Inject the target program account.
        exec_ix.accounts.push(AccountMeta::new_readonly(
            next_instruction.program_id,
            false,
        ));

        // Inject the worker pubkey as the dynamic "payer" account.
        for acc in next_instruction.accounts {
            let acc_pubkey = if acc.pubkey == PAYER_PUBKEY {
                signatory_pubkey
            } else {
                acc.pubkey
            };
            exec_ix.accounts.push(match acc.is_writable {
                true => AccountMeta::new(acc_pubkey, false),
                false => AccountMeta::new_readonly(acc_pubkey, false),
            })
        }
    }

    exec_ix
}
