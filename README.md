# vm-runner

We use [playnet](https://github.com/solana-playground/solana-playground/tree/master/wasm/playnet) as our mini solana vm runtime. Since the core of it is bank, the core problem of using it is fill in bank with proper parameters.

### mini bank context
1. accounts - `HashMap<Pubkey, Account>` or `BTreeMap<Pubkey, Account>`, the execution context needed accounts
2. slot, block_height, genesis_hash, latest_blockhash - current environment parameters
3. builtins - All normal bank's builtin should be here too
4. feature_set - some feature set

Above are basic context needed for mini bank, if it is not enough, we should consider add more bank context from solana repo.
