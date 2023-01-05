extern crate bincode;
extern crate playnet;

use {
    playnet::runtime::bank::PgBank,
    serde::{Deserialize, Serialize},
    solana_program::{clock::INITIAL_RENT_EPOCH, native_token::sol_to_lamports, rent::Rent},
    solana_program_runtime::invoke_context::InvokeContext,
    solana_runtime::genesis_utils::{create_genesis_config_with_leader, GenesisConfigInfo},
    solana_sdk::{
        account::Account,
        hash::Hash,
        instruction::{AccountMeta, Instruction, InstructionError},
        pubkey::Pubkey,
        signature::{Keypair, Signer},
        transaction::{SanitizedTransaction, Transaction},
    },
};

fn main() {
    println!("Hello, world!");

    let GenesisConfigInfo {
        mut genesis_config,
        mint_keypair,
        ..
    } = create_genesis_config_with_leader(sol_to_lamports(100.), &Pubkey::new_unique(), 42);
    genesis_config.rent = Rent::default();

    let mock_program_id = Pubkey::new_unique();
    let account_data_size = 100;
    let rent_exempt_minimum = genesis_config.rent.minimum_balance(account_data_size);

    // Create legacy accounts of various kinds
    let rent_paying_account = Keypair::new();
    genesis_config.accounts.insert(
        rent_paying_account.pubkey(),
        Account::new_rent_epoch(
            rent_exempt_minimum - 1,
            account_data_size,
            &mock_program_id,
            INITIAL_RENT_EPOCH + 1,
        ),
    );
    let rent_exempt_account = Keypair::new();
    genesis_config.accounts.insert(
        rent_exempt_account.pubkey(),
        Account::new_rent_epoch(
            rent_exempt_minimum,
            account_data_size,
            &mock_program_id,
            INITIAL_RENT_EPOCH + 1,
        ),
    );

    let mut bank = PgBank::new(None);
    for (pubkey, account) in genesis_config.accounts.iter() {
        println!("pk: {:?}", pubkey);
        bank.add_account(pubkey, account);
    }
    bank.add_builtin(
        "mock_program",
        &mock_program_id,
        mock_transfer_process_instruction,
    );

    let recent_blockhash = bank.get_latest_blockhash();

    // let check_account_is_rent_exempt = |pubkey: &Pubkey| -> bool {
    //     let account = bank.get_account(pubkey).unwrap();
    //     Rent::default().is_exempt(account.lamports(), account.data().len())
    // };

    // RentPaying account can be left as Uninitialized, in other RentPaying states, or RentExempt
    let tx = create_mock_transfer(
        &mint_keypair,        // payer
        &rent_paying_account, // from
        &mint_keypair,        // to
        1,
        mock_program_id,
        recent_blockhash,
    );
    println!("\n\npayer: {:?}", mint_keypair.pubkey());
    println!("from: {:?}", rent_exempt_account.pubkey());
    println!("to: {:?}", mint_keypair.pubkey());

    println!("Transaction account keys:");
    let _ = tx
        .message
        .account_keys
        .iter()
        .enumerate()
        .map(|(i, k)| {
            println!("{} -- {:?}", i, k);
        })
        .collect::<()>();

    let tx = SanitizedTransaction::try_from_legacy_transaction(tx).unwrap();
    let result = bank.process_tx(tx);
    assert!(result.is_ok());
    // assert!(!check_account_is_rent_exempt(&rent_paying_account.pubkey()));
    println!("all things good!");
}

#[derive(Serialize, Deserialize)]
enum MockTransferInstruction {
    Transfer(u64),
}

fn mock_transfer_process_instruction(
    _first_instruction_account: usize,
    invoke_context: &mut InvokeContext,
) -> Result<(), InstructionError> {
    let transaction_context = &invoke_context.transaction_context;
    let instruction_context = transaction_context.get_current_instruction_context()?;
    let instruction_data = instruction_context.get_instruction_data();
    if let Ok(instruction) = bincode::deserialize(instruction_data) {
        match instruction {
            MockTransferInstruction::Transfer(amount) => {
                instruction_context
                    .try_borrow_instruction_account(transaction_context, 1)?
                    .checked_sub_lamports(amount)?;
                instruction_context
                    .try_borrow_instruction_account(transaction_context, 2)?
                    .checked_add_lamports(amount)?;
                Ok(())
            }
        }
    } else {
        Err(InstructionError::InvalidInstructionData)
    }
}

fn create_mock_transfer(
    payer: &Keypair,
    from: &Keypair,
    to: &Keypair,
    amount: u64,
    mock_program_id: Pubkey,
    recent_blockhash: Hash,
) -> Transaction {
    let account_metas = vec![
        AccountMeta::new(payer.pubkey(), true),
        AccountMeta::new(from.pubkey(), true),
        AccountMeta::new(to.pubkey(), true),
    ];
    let transfer_instruction = Instruction::new_with_bincode(
        mock_program_id,
        &MockTransferInstruction::Transfer(amount),
        account_metas,
    );
    Transaction::new_signed_with_payer(
        &[transfer_instruction],
        Some(&payer.pubkey()),
        &[payer, from, to],
        recent_blockhash,
    )
}
