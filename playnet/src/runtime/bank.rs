use std::{
    cell::RefCell,
    collections::HashMap,
    num::NonZeroUsize,
    rc::Rc,
    sync::{Arc, RwLock},
};

use serde::{Deserialize, Serialize};
use solana_bpf_loader_program::process_instruction as process_bpf_loader_instruction;
use solana_program_runtime::{
    compute_budget::ComputeBudget, executor_cache::Executors,
    invoke_context::{ BuiltinProgram, ProcessInstructionWithContext},  log_collector::LogCollector, sysvar_cache::SysvarCache,
    timings::ExecuteTimings,
};
use solana_sdk::{
    account::{to_account, Account, AccountSharedData, ReadableAccount, WritableAccount},
    account_utils::StateMut,
    bpf_loader,
    bpf_loader_upgradeable::{self, UpgradeableLoaderState},
    clock::Clock,
    feature_set::{self, FeatureSet},
    fee::FeeStructure,
    hash::Hash,
    instruction::CompiledInstruction,
    message::{
        v0::{LoadedAddresses, MessageAddressTableLookup},
        AddressLoaderError, SanitizedMessage,
    },
    ed25519_program,
    secp256k1_program,
    native_loader,
    pubkey::Pubkey,
    rent::Rent,
    signature::Signature,
    slot_history::Slot,
    system_program,
    sysvar::{self, instructions::{construct_instructions_data}, Sysvar},
    transaction::{self, AddressLoader, SanitizedTransaction, TransactionError},
    transaction_context::{
        ExecutionRecord, TransactionAccount, TransactionContext,
        TransactionReturnData,
    },
};

use crate::{serde::bank_accounts, types::SimulateTransactionResult, utils::create_blockhash};

use super::{
    message_processor::MessageProcessor,
    system_instruction_processor::{
        get_system_account_kind, process_system_instruction, SystemAccountKind,
    },
    transaction_history::{ConfirmedTransactionMeta, TransactionData},
};

#[derive(Serialize, Deserialize)]
pub struct PgBank {
    /// Where all the accounts are stored
    #[serde(with = "bank_accounts")]
    accounts: BankAccounts,

    /// Where all the transactions are stored.
    ///
    /// Currently transactions are only
    /// getting stored in the memory and not in IndexedDB for both to not use
    /// unnecessary space since txs are the biggest contributing factor to the
    /// size of the bank and because `VersionedMessage` is not getting properly
    /// de-serialized.
    #[serde(skip)]
    txs: BankTxs,

    /// Bank's slot (i.e. block)
    slot: Slot,

    /// Bank's block height
    block_height: u64,

    /// Bank's first hash
    genesis_hash: Hash,

    /// Bank's latest blockhash
    latest_blockhash: Hash,

    /// Essential programs that don't get deployed with transactions
    #[serde(skip)]
    builtin_programs: Vec<BuiltinProgram>,

    /// Where all the sysvars are stored
    #[serde(skip)]
    sysvar_cache: RwLock<SysvarCache>,

    /// Active/inactive features(not able to change yet)
    #[serde(skip)]
    feature_set: Rc<FeatureSet>,
}

impl PgBank {
    const LAMPORTS_PER_SIGNATURE: u64 = 0;

    pub fn new(maybe_bank_string: Option<String>) -> Self {
        let bank = match maybe_bank_string {
            Some(bank_string) => serde_json::from_str::<Self>(&bank_string).unwrap(),
            None => {
                let genesis_hash = create_blockhash(b"playnet");
                Self {
                    accounts: HashMap::new(),
                    txs: HashMap::new(),
                    slot: 0,
                    block_height: 0,
                    genesis_hash,
                    latest_blockhash: genesis_hash,
                    builtin_programs: vec![],
                    sysvar_cache: RwLock::new(SysvarCache::default()),
                    feature_set: Rc::new(FeatureSet::default()),
                }
            }
        };

        bank.init()
    }

    pub fn new_with_more(accounts: BankAccounts, genesis_hash: Hash) -> Self {
        let bank = Self {
            accounts,
            txs: HashMap::new(),
            slot: 0,
            block_height: 0,
            genesis_hash,
            latest_blockhash: genesis_hash,
            builtin_programs: vec![],
            sysvar_cache: RwLock::new(SysvarCache::default()),
            feature_set: Rc::new(FeatureSet::default()),
        };

        bank.init()
    }

    fn init(mut self) -> Self {
        // Add native accounts
        let mut add_native_programs = |program_id: Pubkey| {
            let mut account = Account::new(1, 0, &native_loader::id());
            account.set_executable(true);
            self.accounts.insert(program_id, account);
        };

        add_native_programs(bpf_loader::id());
        add_native_programs(bpf_loader_upgradeable::id());
        add_native_programs(system_program::id());

        // Add sysvar accounts
        fn add_sysvar_account<S: Sysvar>(bank: &mut PgBank) -> S {
            let default = S::default();
            let mut account = Account::new(
                1,
                bincode::serialized_size(&default).unwrap() as usize,
                &sysvar::id(),
            );
            to_account(&default, &mut account).unwrap();
            bank.accounts.insert(S::id(), account);

            default
        }

        let clock = add_sysvar_account::<Clock>(&mut self);
        let rent = add_sysvar_account::<Rent>(&mut self);
        let mut sysvar_cache = self.sysvar_cache.write().unwrap();
        sysvar_cache.set_clock(clock);
        sysvar_cache.set_rent(rent);
        drop(sysvar_cache);

        // Add builtin programs
        self.builtin_programs = vec![
            BuiltinProgram {
                program_id: bpf_loader::id(),
                process_instruction: process_bpf_loader_instruction,
            },
            BuiltinProgram {
                program_id: bpf_loader_upgradeable::id(),
                process_instruction: process_bpf_loader_instruction,
            },
            BuiltinProgram {
                program_id: system_program::id(),
                process_instruction: process_system_instruction,
            },
        ];

        // Feature set
        self.feature_set = Rc::new(FeatureSet::default());

        self
    }

    pub fn add_account(&mut self, key: &Pubkey, account: &Account) {
        self.accounts.insert(key.clone(), account.clone());
    }

    pub fn add_builtin(&mut self, name: &str, program_id: &Pubkey, instructions: ProcessInstructionWithContext) {
        self.builtin_programs.push(BuiltinProgram {
            program_id: program_id.clone(),
            process_instruction: instructions,
        });

        let account = native_loader::create_loadable_account_with_fields(
            name,
            (1000000000, 1000),
        );
        self.add_account(program_id, &Account::from(account));
    }

    pub fn get_slot(&self) -> Slot {
        self.slot
    }

    pub fn get_block_height(&self) -> u64 {
        self.block_height
    }

    pub fn get_genesis_hash(&self) -> Hash {
        self.latest_blockhash
    }

    pub fn get_latest_blockhash(&self) -> Hash {
        self.latest_blockhash
    }

    pub fn get_minimum_balance_for_rent_exemption(&self, data_len: usize) -> u64 {
        Rent::default().minimum_balance(data_len).max(1)
    }

    pub fn feature_set(&self) -> &FeatureSet {
        &*self.feature_set
    }

    /// Returns `None` for accounts with 0 lamports
    pub fn get_account(&self, pubkey: &Pubkey) -> Option<&Account> {
        self.accounts.get(pubkey)
    }

    /// Returns `Account::default` for 0 lamports account
    pub fn get_account_default(&self, pubkey: &Pubkey) -> Account {
        match self.accounts.get(pubkey) {
            Some(account) => account.to_owned(),
            None => Account::default(),
        }
    }

    /// Inserts the account if it doesn't exist or updates the existing account.
    /// Previous value or `None` is returned for initial insertion.
    pub fn set_account(&mut self, pubkey: Pubkey, account: Account) -> Option<Account> {
        self.accounts.insert(pubkey, account)
    }

    pub fn get_fee_for_message(&self, msg: &SanitizedMessage) -> Option<u64> {
        (msg.header().num_required_signatures.max(1) as u64)
            .checked_mul(PgBank::LAMPORTS_PER_SIGNATURE)
    }

    fn get_num_signatures_in_message(message: &SanitizedMessage) -> u64 {
        let mut num_signatures = u64::from(message.header().num_required_signatures);
        // This next part is really calculating the number of pre-processor
        // operations being done and treating them like a signature
        for (program_id, instruction) in message.program_instructions_iter() {
            if secp256k1_program::check_id(program_id) || ed25519_program::check_id(program_id) {
                if let Some(num_verifies) = instruction.data.first() {
                    num_signatures = num_signatures.saturating_add(u64::from(*num_verifies));
                }
            }
        }
        num_signatures
    }

    fn get_num_write_locks_in_message(message: &SanitizedMessage) -> u64 {
        message
            .account_keys()
            .len()
            .saturating_sub(message.num_readonly_accounts()) as u64
    }

    /// Calculate fee for `SanitizedMessage`
    pub fn calculate_fee(
        message: &SanitizedMessage,
        lamports_per_signature: u64,
        fee_structure: &FeeStructure,
        support_set_compute_unit_price_ix: bool,
        use_default_units_per_instruction: bool,
    ) -> u64 {
        // Fee based on compute units and signatures
        const BASE_CONGESTION: f64 = 5_000.0;
        let current_congestion = BASE_CONGESTION.max(lamports_per_signature as f64);
        let congestion_multiplier = if lamports_per_signature == 0 {
            0.0 // test only
        } else {
            BASE_CONGESTION / current_congestion
        };

        let mut compute_budget = ComputeBudget::default();
        let prioritization_fee_details = compute_budget
            .process_instructions(
                message.program_instructions_iter(),
                use_default_units_per_instruction,
                support_set_compute_unit_price_ix,
            )
            .unwrap_or_default();
        let prioritization_fee = prioritization_fee_details.get_fee();
        let signature_fee = Self::get_num_signatures_in_message(message)
            .saturating_mul(fee_structure.lamports_per_signature);
        let write_lock_fee = Self::get_num_write_locks_in_message(message)
            .saturating_mul(fee_structure.lamports_per_write_lock);
        let compute_fee = fee_structure
            .compute_fee_bins
            .iter()
            .find(|bin| compute_budget.compute_unit_limit <= bin.limit)
            .map(|bin| bin.fee)
            .unwrap_or_else(|| {
                fee_structure
                    .compute_fee_bins
                    .last()
                    .map(|bin| bin.fee)
                    .unwrap_or_default()
            });

        ((prioritization_fee
            .saturating_add(signature_fee)
            .saturating_add(write_lock_fee)
            .saturating_add(compute_fee) as f64)
            * congestion_multiplier)
            .round() as u64
    }

    pub fn simulate_tx(&self, tx: &SanitizedTransaction) -> SimulateTransactionResult {
        // let fee = calculate_fee(tx.message());
        let mut loaded_tx = match self.load_tx(tx) {
            Ok(loaded_tx) => loaded_tx,
            Err(err) => return SimulateTransactionResult::new_error(err),
        };

        let account_count = tx.message().account_keys().len();
        let pre_accounts = loaded_tx
            .accounts
            .clone()
            .into_iter()
            .take(account_count)
            .collect::<Vec<TransactionAccount>>();

        match self.execute_loaded_tx(&tx, &mut loaded_tx) {
            TransactionExecutionResult::Executed {
                details,
                tx_executor_cache: _,
            } => SimulateTransactionResult::new(
                details.status,
                pre_accounts,
                loaded_tx.accounts.into_iter().take(account_count).collect(),
                details.log_messages.unwrap_or_default(),
                details.executed_units,
                details.return_data,
            ),
            TransactionExecutionResult::NotExecuted(err) => {
                SimulateTransactionResult::new_error(err)
            }
        }
    }

    pub fn process_tx(&mut self, tx: SanitizedTransaction) -> transaction::Result<Signature> {
        let simulation_result = self.simulate_tx(&tx);
        match simulation_result.result {
            Ok(_) => {
                // TODO: Substract the fee from the `fee_payer`
                let fee = self.get_fee_for_message(tx.message()).unwrap();

                for (pubkey, account) in &simulation_result.post_accounts {
                    self.set_account(pubkey.clone(), account.clone().into());
                }

                let tx_hash = self.save_tx(tx, simulation_result, fee)?;
                Ok(tx_hash)
            }
            Err(err) => Err(err),
        }
    }

    pub fn get_tx(&self, signature: &Signature) -> Option<&TransactionData> {
        self.txs.get(signature)
    }

    fn new_slot(&mut self) {
        self.latest_blockhash = create_blockhash(&self.latest_blockhash.to_bytes());
        self.slot += 1;
        self.block_height += 1;
    }

    fn save_tx(
        &mut self,
        tx: SanitizedTransaction,
        result: SimulateTransactionResult,
        fee: u64,
    ) -> transaction::Result<Signature> {
        let signature = tx.signature();

        // Don't save BPF Upgradeable Loader Write ix as its mostly wasted space
        let bpf_upgradeable_write_ix_exists = tx
            .message()
            .instructions()
            .iter()
            .find(|ix| {
                let program_id = tx
                    .message()
                    .account_keys()
                    .get(ix.program_id_index as usize)
                    .unwrap();

                *program_id == bpf_loader_upgradeable::id() && ix.data.starts_with(&[1])
            })
            .is_some();
        if bpf_upgradeable_write_ix_exists {
            return Ok(signature.to_owned());
        }

        // Get whether the tx signature already exists
        match self.txs.get(signature) {
            Some(_) => Err(TransactionError::AlreadyProcessed),
            None => {
                let signature = signature.to_owned();
                self.txs.insert(
                    signature,
                    TransactionData::new(
                        self.get_slot(),
                        tx.to_versioned_transaction(),
                        Some(ConfirmedTransactionMeta {
                            fee,
                            // TODO:
                            inner_instructions: None,
                            pre_balances: result
                                .pre_accounts
                                .iter()
                                .map(|(_, data)| data.lamports())
                                .collect(),
                            post_balances: result
                                .post_accounts
                                .iter()
                                .map(|(_, data)| data.lamports())
                                .collect(),
                            log_messages: Some(result.logs),
                            // TODO:
                            pre_token_balances: None,
                            // TODO:
                            post_token_balances: None,
                            err: result.result.err(),
                            loaded_addresses: None,
                            compute_units_consumed: Some(result.units_consumed),
                        }),
                        Some(
                            self.sysvar_cache
                                .read()
                                .unwrap()
                                .get_clock()
                                .unwrap()
                                .unix_timestamp,
                        ),
                    ),
                );
                self.new_slot();

                Ok(signature)
            }
        }
    }

    fn load_tx(&self, tx: &SanitizedTransaction) -> transaction::Result<LoadedTransaction> {
        let fee = 0;
        let mut error_counters = TransactionErrorMetrics::default();
        let feature_set = FeatureSet::default();
        self.load_tx_accounts(&tx, fee, &mut error_counters, &feature_set)
    }

    fn execute_loaded_tx(
        &self,
        tx: &SanitizedTransaction,
        loaded_tx: &mut LoadedTransaction,
    ) -> TransactionExecutionResult {
        let compute_budget = ComputeBudget::default();
        let mut transaction_context = TransactionContext::new(
            loaded_tx.accounts.clone(),
            None,
            compute_budget.max_call_depth,
            compute_budget.max_invoke_depth,
        );

        let log_collector = Rc::new(RefCell::new(LogCollector::default()));
        let tx_executor_cache = Rc::new(RefCell::new(Executors::default()));
        let feature_set = Arc::new(FeatureSet::default());
        let mut timings = ExecuteTimings::default();
        let blockhash = tx.message().recent_blockhash();
        let current_accounts_data_len = u32::MAX as u64;
        let mut accumulated_consume_units = 0;

        // Get sysvars
        let sysvar_cache = self.sysvar_cache.read().unwrap();

        let process_result = MessageProcessor::process_message(
            &self.builtin_programs,
            tx.message(),
            &loaded_tx.program_indices,
            &mut transaction_context,
            *sysvar_cache.get_rent().unwrap(),
            Some(Rc::clone(&log_collector)),
            Rc::clone(&tx_executor_cache),
            feature_set,
            compute_budget,
            &mut timings,
            &sysvar_cache,
            *blockhash,
            PgBank::LAMPORTS_PER_SIGNATURE,
            current_accounts_data_len,
            &mut accumulated_consume_units,
        );

        let ExecutionRecord {
            accounts,
            instruction_trace: _,
            mut return_data,
            changed_account_count: _,
            total_size_of_all_accounts: _,
            total_size_of_touched_accounts: _,
            accounts_resize_delta: _,
        } = transaction_context.into();
        loaded_tx.accounts = accounts;

        match process_result {
            Ok(info) => TransactionExecutionResult::Executed {
                details: TransactionExecutionDetails {
                    status: Ok(()),
                    log_messages: Some(log_collector.borrow().get_recorded_content().to_vec()),
                    inner_instructions: None,
                    durable_nonce_fee: None,
                    return_data: match return_data.data.iter().rposition(|&x| x != 0) {
                        Some(end_index) => {
                            let end_index = end_index.saturating_add(1);
                            return_data.data.truncate(end_index);
                            Some(return_data)
                        }
                        None => None,
                    },
                    executed_units: accumulated_consume_units,
                    accounts_data_len_delta: info.accounts_data_len_delta,
                },
                tx_executor_cache,
            },
            Err(err) => TransactionExecutionResult::NotExecuted(err),
        }
    }

    fn load_tx_accounts(
        &self,
        tx: &SanitizedTransaction,
        fee: u64,
        error_counters: &mut TransactionErrorMetrics,
        feature_set: &FeatureSet,
    ) -> transaction::Result<LoadedTransaction> {
        // NOTE: this check will never fail because `tx` is sanitized
        if tx.signatures().is_empty() && fee != 0 {
            return Err(TransactionError::MissingSignatureForFee);
        }

        // There is no way to predict what program will execute without an error
        // If a fee can pay for execution then the program will be scheduled
        let mut validated_fee_payer = false;
        let message = tx.message();
        let account_keys = message.account_keys();
        let mut account_deps = Vec::with_capacity(account_keys.len());
        let requested_loaded_accounts_data_size_limit = None;

        let mut accumulated_accounts_data_size: usize = 0;

        let mut accounts = account_keys
            .iter()
            .enumerate()
            .map(|(i, pubkey)| {
                println!("index: {}, pk: {:?}", i, pubkey);
                let (account, loaded_programdata_account_size) = if !message.is_non_loader_key(i) {
                    // TODO:
                    // Fill in an empty account for the program slots.
                    // (AccountSharedData::default(), 0)
                    let account = self.get_account_default(pubkey);
                    let program_len = account.data.len();
                    (AccountSharedData::from(account), program_len)
                } else {
                    if solana_sdk::sysvar::instructions::check_id(pubkey) {
                        (
                            Self::construct_instructions_account(
                                message,
                                feature_set.is_active(
                                    &feature_set::instructions_sysvar_owned_by_sysvar::id(),
                                ),
                            ),
                            0,
                        )
                    } else {
                        let mut account = AccountSharedData::from(self.get_account_default(pubkey));

                        if !validated_fee_payer {
                            Self::validate_fee_payer(
                                pubkey,
                                &mut account,
                                i,
                                error_counters,
                                feature_set,
                                fee,
                            )?;

                            validated_fee_payer = true;
                        }

                        let mut loaded_programdata_account_size: usize = 0;
                        if bpf_loader_upgradeable::check_id(account.owner()) {
                            if message.is_writable(i) && !message.is_upgradeable_loader_present() {
                                error_counters.invalid_writable_account += 1;
                                return Err(TransactionError::InvalidWritableAccount);
                            }

                            if account.executable() {
                                // The upgradeable loader requires the derived ProgramData account
                                if let Ok(UpgradeableLoaderState::Program {
                                    programdata_address,
                                }) = account.state()
                                {
                                    if let Some(programdata_account) =
                                        self.get_account(&programdata_address)
                                    {
                                        loaded_programdata_account_size =
                                            programdata_account.data().len();
                                        account_deps.push((
                                            programdata_address,
                                            AccountSharedData::from(programdata_account.to_owned()),
                                        ));
                                    } else {
                                        error_counters.account_not_found += 1;
                                        return Err(TransactionError::ProgramAccountNotFound);
                                    }
                                } else {
                                    error_counters.invalid_program_for_execution += 1;
                                    return Err(TransactionError::InvalidProgramForExecution);
                                }
                            }
                        } else if account.executable() && message.is_writable(i) {
                            error_counters.invalid_writable_account += 1;
                            return Err(TransactionError::InvalidWritableAccount);
                        }

                        (account, loaded_programdata_account_size)
                    }
                };
                Self::accumulate_and_check_loaded_account_data_size(
                    &mut accumulated_accounts_data_size,
                    account
                        .data()
                        .len()
                        .saturating_add(loaded_programdata_account_size),
                    requested_loaded_accounts_data_size_limit,
                    error_counters,
                )?;

                Ok((*pubkey, account))
            })
            .collect::<transaction::Result<Vec<_>>>()?;

        // Appends the account_deps at the end of the accounts,
        // this way they can be accessed in a uniform way.
        // At places where only the accounts are needed,
        // the account_deps are truncated using e.g:
        // accounts.iter().take(message.account_keys.len())
        accounts.append(&mut account_deps);

        println!("accounts[0]: {:?}", accounts[0].0);
        println!("accounts[1]: {:?}", accounts[1].0);
        println!("accounts[2]: {:?}", accounts[2].0);

        if validated_fee_payer {
            let program_indices = message
                .instructions()
                .iter()
                .map(|instruction| {
                    self.load_executable_accounts(
                        &mut accounts,
                        instruction.program_id_index as usize,
                        error_counters,
                        &mut accumulated_accounts_data_size,
                        requested_loaded_accounts_data_size_limit,
                    )
                })
                .collect::<transaction::Result<Vec<Vec<usize>>>>()?;

            Ok(LoadedTransaction {
                accounts,
                program_indices,
            })
        } else {
            error_counters.account_not_found += 1;
            Err(TransactionError::AccountNotFound)
        }
    }

    fn load_executable_accounts(
        &self,
        accounts: &mut Vec<TransactionAccount>,
        mut program_account_index: usize,
        error_counters: &mut TransactionErrorMetrics,
        accumulated_accounts_data_size: &mut usize,
        requested_loaded_accounts_data_size_limit: Option<NonZeroUsize>,
    ) -> transaction::Result<Vec<usize>> {
        let mut account_indices = Vec::new();
        let (mut program_id, already_loaded_as_non_loader) =
            match accounts.get(program_account_index as usize) {
                Some(program_account) => (
                    program_account.0,
                    // program account is already loaded if it's not empty in `accounts`
                    program_account.1 != AccountSharedData::default(),
                ),
                None => {
                    error_counters.account_not_found += 1;
                    return Err(TransactionError::ProgramAccountNotFound);
                }
            };
        println!("program_id: {:?}", program_id);
        let mut depth = 0;
        while !native_loader::check_id(&program_id) {
            if depth >= 5 {
                error_counters.call_chain_too_deep += 1;
                return Err(TransactionError::CallChainTooDeep);
            }
            depth += 1;
            let mut loaded_account_total_size: usize = 0;

            program_account_index = match self.get_account(&program_id) {
                Some(program_account) => {
                    let account_index = accounts.len();
                    // do not double count account size for program account on top of call chain
                    // that has already been loaded during load_tx as non-loader account.
                    // Other accounts data size in the call chain are counted.
                    if !(depth == 1 && already_loaded_as_non_loader) {
                        loaded_account_total_size =
                            loaded_account_total_size.saturating_add(program_account.data().len());
                    }
                    accounts.push((
                        program_id,
                        AccountSharedData::from(program_account.to_owned()),
                    ));
                    account_index
                }
                None => {
                    error_counters.account_not_found += 1;
                    return Err(TransactionError::ProgramAccountNotFound);
                }
            };
            let program = &accounts[program_account_index as usize].1;
            if !program.executable() {
                error_counters.invalid_program_for_execution += 1;
                return Err(TransactionError::InvalidProgramForExecution);
            }

            // Add loader to chain
            let program_owner = *program.owner();
            account_indices.insert(0, program_account_index);

            if bpf_loader_upgradeable::check_id(&program_owner) {
                // The upgradeable loader requires the derived ProgramData account
                if let Ok(UpgradeableLoaderState::Program {
                    programdata_address,
                }) = program.state()
                {
                    let programdata_account_index = match self.get_account(&programdata_address) {
                        Some(programdata_account) => {
                            let account_index = accounts.len();
                            if !(depth == 1 && already_loaded_as_non_loader) {
                                loaded_account_total_size = loaded_account_total_size
                                    .saturating_add(programdata_account.data().len());
                            }
                            accounts.push((
                                programdata_address,
                                AccountSharedData::from(programdata_account.to_owned()),
                            ));
                            account_index
                        }
                        None => {
                            error_counters.account_not_found += 1;
                            return Err(TransactionError::ProgramAccountNotFound);
                        }
                    };
                    account_indices.insert(0, programdata_account_index);
                } else {
                    error_counters.invalid_program_for_execution += 1;
                    return Err(TransactionError::InvalidProgramForExecution);
                }
            }
            Self::accumulate_and_check_loaded_account_data_size(
                accumulated_accounts_data_size,
                loaded_account_total_size,
                requested_loaded_accounts_data_size_limit,
                error_counters,
            )?;

            program_id = program_owner;
        }
        Ok(account_indices)
    }

    fn construct_instructions_account(
        message: &SanitizedMessage,
        is_owned_by_sysvar: bool,
    ) -> AccountSharedData {
        let data = construct_instructions_data(&message.decompile_instructions());
        let owner = if is_owned_by_sysvar {
            sysvar::id()
        } else {
            system_program::id()
        };
        AccountSharedData::from(Account {
            data,
            owner,
            ..Account::default()
        })
    }

    fn validate_fee_payer(
        _payer_address: &Pubkey,
        payer_account: &mut AccountSharedData,
        _payer_index: usize,
        error_counters: &mut TransactionErrorMetrics,
        _feature_set: &FeatureSet,
        fee: u64,
    ) -> transaction::Result<()> {
        if payer_account.lamports() == 0 {
            error_counters.account_not_found += 1;
            return Err(TransactionError::AccountNotFound);
        }
        let min_balance = match get_system_account_kind(payer_account).ok_or_else(|| {
            error_counters.invalid_account_for_fee += 1;
            TransactionError::InvalidAccountForFee
        })? {
            SystemAccountKind::System => 0,
            SystemAccountKind::Nonce => todo!(),
        };

        if payer_account.lamports() < fee + min_balance {
            error_counters.insufficient_funds += 1;
            return Err(TransactionError::InsufficientFundsForFee);
        }

        Ok(())
    }

    fn accumulate_and_check_loaded_account_data_size(
        accumulated_loaded_accounts_data_size: &mut usize,
        account_data_size: usize,
        requested_loaded_accounts_data_size_limit: Option<NonZeroUsize>,
        error_counters: &mut TransactionErrorMetrics,
    ) -> transaction::Result<()> {
        if let Some(requested_loaded_accounts_data_size) = requested_loaded_accounts_data_size_limit
        {
            *accumulated_loaded_accounts_data_size =
                accumulated_loaded_accounts_data_size.saturating_add(account_data_size);
            if *accumulated_loaded_accounts_data_size > requested_loaded_accounts_data_size.get() {
                error_counters.max_loaded_accounts_data_size_exceeded += 1;
                Err(TransactionError::WouldExceedAccountDataTotalLimit)
            } else {
                Ok(())
            }
        } else {
            Ok(())
        }
    }
}

/// Mapping between Pubkeys and Accounts
pub type BankAccounts = HashMap<Pubkey, Account>;

/// Mapping between Signatures and TransactionData
pub type BankTxs = HashMap<Signature, TransactionData>;

/// Filler struct, address loader is not yet implemented
#[derive(Clone, Default)]
pub struct PgAddressLoader {}

impl AddressLoader for PgAddressLoader {
    fn load_addresses(
        self,
        _lookups: &[MessageAddressTableLookup],
    ) -> Result<LoadedAddresses, AddressLoaderError> {
        Err(AddressLoaderError::Disabled)
    }
}

type TransactionProgramIndices = Vec<Vec<usize>>;

#[derive(PartialEq, Eq, Debug, Clone)]
struct LoadedTransaction {
    pub accounts: Vec<TransactionAccount>,
    pub program_indices: TransactionProgramIndices,
}

/// Type safe representation of a transaction execution attempt which
/// differentiates between a transaction that was executed (will be
/// committed to the ledger) and a transaction which wasn't executed
/// and will be dropped.
///
/// Note: `Result<TransactionExecutionDetails, TransactionError>` is not
/// used because it's easy to forget that the inner `details.status` field
/// is what should be checked to detect a successful transaction. This
/// enum provides a convenience method `Self::was_executed_successfully` to
/// make such checks hard to do incorrectly.
#[derive(Clone)]
pub enum TransactionExecutionResult {
    Executed {
        details: TransactionExecutionDetails,
        tx_executor_cache: Rc<RefCell<Executors>>,
    },
    NotExecuted(TransactionError),
}

#[derive(Clone)]
pub struct TransactionExecutionDetails {
    pub status: transaction::Result<()>,
    pub log_messages: Option<Vec<String>>,
    pub inner_instructions: Option<InnerInstructionsList>,
    pub durable_nonce_fee: Option<DurableNonceFee>,
    pub return_data: Option<TransactionReturnData>,
    pub executed_units: u64,
    /// The change in accounts data len for this transaction.
    /// NOTE: This value is valid if `status` is `Ok`.
    pub accounts_data_len_delta: i64,
}

#[allow(dead_code)]
#[derive(Clone)]
pub enum DurableNonceFee {
    Valid(u64),
    Invalid,
}

/// A list of compiled instructions that were invoked during each instruction of
/// a transaction
pub type InnerInstructionsList = Vec<InnerInstructions>;

/// An ordered list of compiled instructions that were invoked during a
/// transaction instruction
pub type InnerInstructions = Vec<InnerInstruction>;

#[derive(Clone, PartialEq, Eq)]
pub struct InnerInstruction {
    pub instruction: CompiledInstruction,
    /// Invocation stack height of this instruction. Instruction stack height
    /// starts at 1 for transaction instructions.
    pub stack_height: u8,
}

#[derive(Default)]
pub struct TransactionErrorMetrics {
    pub total: usize,
    pub account_in_use: usize,
    pub too_many_account_locks: usize,
    pub account_loaded_twice: usize,
    pub account_not_found: usize,
    pub blockhash_not_found: usize,
    pub blockhash_too_old: usize,
    pub call_chain_too_deep: usize,
    pub already_processed: usize,
    pub instruction_error: usize,
    pub insufficient_funds: usize,
    pub invalid_account_for_fee: usize,
    pub invalid_account_index: usize,
    pub invalid_program_for_execution: usize,
    pub not_allowed_during_cluster_maintenance: usize,
    pub invalid_writable_account: usize,
    pub invalid_rent_paying_account: usize,
    pub would_exceed_max_block_cost_limit: usize,
    pub would_exceed_max_account_cost_limit: usize,
    pub would_exceed_max_vote_cost_limit: usize,
    pub would_exceed_account_data_block_limit: usize,
    pub max_loaded_accounts_data_size_exceeded: usize,
    pub invalid_loaded_accounts_data_size_limit: usize,
}
