use crate::{interpreter::bytecode::Bytecode, models::SelfDestructResult, Return, KECCAK_EMPTY};
use alloc::{vec, vec::Vec};
use core::mem::{self};
use hashbrown::{hash_map::Entry, HashMap as Map};
use primitive_types::{H160, U256};

use crate::{db::Database, AccountInfo, Log};

#[derive(Debug, Clone)]
pub struct JournaledState {
    /// Current state.
    pub state: State,
    /// logs
    pub logs: Vec<Log>,
    /// how deep are we in call stack.
    pub depth: usize,
    /// journal with changes that happened between calls.
    pub journal: Vec<Vec<JournalEntry>>,
}

pub type State = Map<H160, Account>;
pub type Storage = Map<U256, StorageSlot>;

#[derive(Debug, Clone)]
pub struct Account {
    /// Balance of the account.
    pub info: AccountInfo,
    /// storage cache
    pub storage: Map<U256, StorageSlot>,
    /// If account is newly created, we will not ask database for storage values
    pub storage_cleared: bool,
    /// if account is destroyed it will be scheduled for removal.
    pub is_destroyed: bool,
    /// if account is touched
    pub is_touched: bool,
    /// is precompile
    pub is_existing_precompile: bool,
}

impl Account {
    pub fn is_empty(&self) -> bool {
        self.info.is_empty()
    }
}

impl From<AccountInfo> for Account {
    fn from(info: AccountInfo) -> Self {
        Self {
            info,
            storage: Map::new(),
            storage_cleared: false,
            is_destroyed: false,
            is_touched: false,
            is_existing_precompile: false,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct StorageSlot {
    original_value: U256,
    /// When loaded with sload present value is set to original value
    present_value: U256,
}

impl StorageSlot {
    pub fn new(original: U256) -> Self {
        Self {
            original_value: original,
            present_value: original,
        }
    }
    pub fn present_value(&self) -> U256 {
        self.present_value
    }
}

#[derive(Debug, Clone)]
pub enum JournalEntry {
    /// Used to mark account that is hot inside EVM in regards to EIP-2929 AccessList.
    /// Action: We will add Account to state.
    /// Revert: we will remove account from state.
    AccountLoaded { address: H160 },
    /// Mark account to be destroyed and journal balance to be reverted
    /// Action: Mark account and transfer the balance
    /// Revert: Unmark the account and transfer balance back
    AccountDestroyed {
        address: H160,
        target: H160,
        was_destroyed: bool, // if account had already been destroyed before this journal entry
        had_balance: U256,
    },
    /// Loading account does not mean that account will need to be added to MerkleTree (touched).
    /// Only when account is called (to execute contract or transfer balance) only then account is made touched.
    /// Action: Mark account touched
    /// Revert: Unmark account touched
    AccountTouched { address: H160 },
    /// Transfer balance between two accounts
    /// Action: Transfer balance
    /// Revert: Transfer balance back
    BalanceTransfer { from: H160, to: H160, balance: U256 },
    /// Increment nonce
    /// Action: Increment nonce by one
    /// Revert: Decrement nonce by one
    NonceChange {
        address: H160, //geth has nonce value,
    },
    /// It is used to track both storage change and hot load of storage slot. For hot load in regards
    /// to EIP-2929 AccessList had_value will be None
    /// Action: Storage change or hot load
    /// Revert: Revert to previous value or remove slot from storage
    StorageChage {
        address: H160,
        key: U256,
        had_value: Option<U256>, //if none, storage slot was cold loaded from db and needs to be removed
    },
    /// Code changed
    /// Action: Account code changed
    /// Revert: Revert to previous bytecode.
    CodeChange { address: H160, had_code: Bytecode },
}

/// SubRoutine checkpoint that will help us to go back from this
pub struct JournalCheckpoint {
    log_i: usize,
    journal_i: usize,
}

impl Default for JournaledState {
    fn default() -> Self {
        Self::new()
    }
}

impl JournaledState {
    pub fn new() -> JournaledState {
        Self {
            state: Map::new(),
            logs: Vec::new(),
            journal: vec![vec![]],
            depth: 0,
        }
    }

    pub fn state(&mut self) -> &mut State {
        &mut self.state
    }

    pub fn touch(&mut self, address: &H160) {
        if let Some(account) = self.state.get_mut(address) {
            Self::touch_account(self.journal.last_mut().unwrap(), address, account);
        }
    }

    fn touch_account(journal: &mut Vec<JournalEntry>, address: &H160, account: &mut Account) {
        if !account.is_touched {
            journal.push(JournalEntry::AccountTouched { address: *address });
            account.is_touched = true;
        }
    }

    pub fn load_precompiles(&mut self, precompiles: Map<H160, AccountInfo>) {
        let state: Map<H160, Account> = precompiles
            .into_iter()
            .map(|(k, value)| {
                let acc = Account::from(value);
                (k, acc)
            })
            .collect();
        self.state.extend(state);
    }

    pub fn load_precompiles_default(&mut self, precompiles: &[H160]) {
        let state: State = precompiles
            .iter()
            .map(|&k| {
                let mut acc = Account::from(AccountInfo::default());
                acc.is_existing_precompile = true;
                (k, acc)
            })
            .collect();
        self.state.extend(state);
    }

    /// do cleanup and return modified state
    pub fn finalize(&mut self) -> (State, Vec<Log>) {
        let state = mem::take(&mut self.state);

        let state = state
            .into_iter()
            .filter(|(_, account)| account.is_touched)
            .collect();

        let logs = mem::take(&mut self.logs);
        self.journal = vec![vec![]];
        self.depth = 0;
        (state, logs)
    }

    /// Use it with load_account function.
    pub fn account(&self, address: H160) -> &Account {
        self.state.get(&address).unwrap() // Always assume that acc is already loaded
    }

    pub fn depth(&self) -> u64 {
        self.depth as u64
    }

    /// use it only if you know that acc is hot
    /// Assume account is hot
    pub fn set_code(&mut self, address: H160, code: Bytecode) {
        let account = self.state.get_mut(&address).unwrap();
        Self::touch_account(self.journal.last_mut().unwrap(), &address, account);

        self.journal
            .last_mut()
            .unwrap()
            .push(JournalEntry::CodeChange {
                address,
                had_code: code.clone(),
            });

        account.info.code_hash = code.hash();
        account.info.code = Some(code);
    }

    pub fn inc_nonce(&mut self, address: H160) -> Option<u64> {
        let account = self.state.get_mut(&address).unwrap();
        // Check if nonce is going to overflow.
        if account.info.nonce == u64::MAX {
            return None;
        }
        Self::touch_account(self.journal.last_mut().unwrap(), &address, account);
        self.journal
            .last_mut()
            .unwrap()
            .push(JournalEntry::NonceChange { address });

        account.info.nonce += 1;

        Some(account.info.nonce)
    }

    pub fn transfer<DB: Database>(
        &mut self,
        from: &H160,
        to: &H160,
        balance: U256,
        db: &mut DB,
    ) -> Result<(bool, bool), Return> {
        // load accounts
        let from_is_cold = self.load_account(*from, db);
        let to_is_cold = self.load_account(*to, db);

        // sub balance from
        let from_account = &mut self.state.get_mut(from).unwrap();
        Self::touch_account(self.journal.last_mut().unwrap(), from, from_account);
        let from_balance = &mut from_account.info.balance;
        *from_balance = from_balance.checked_sub(balance).ok_or(Return::OutOfFund)?;

        // add balance to
        let to_account = &mut self.state.get_mut(to).unwrap();
        Self::touch_account(self.journal.last_mut().unwrap(), to, to_account);
        let to_balance = &mut to_account.info.balance;
        *to_balance = to_balance
            .checked_add(balance)
            .ok_or(Return::OverflowPayment)?;
        // Overflow of U256 balance is not possible to happen on mainnet. We dont bother to return funds from from_acc.

        self.journal
            .last_mut()
            .unwrap()
            .push(JournalEntry::BalanceTransfer {
                from: *from,
                to: *to,
                balance,
            });

        Ok((from_is_cold, to_is_cold))
    }

    /// return if it has collision of addresses
    pub fn create_account<DB: Database>(
        &mut self,
        address: H160,
        is_precompile: bool,
        db: &mut DB,
    ) -> bool {
        let (acc, _) = self.load_code(address, db);

        // Check collision. Bytecode needs to be empty.
        if let Some(ref code) = acc.info.code {
            if !code.is_empty() {
                return false;
            }
        }
        // Check collision. Nonce is not zero
        if acc.info.nonce != 0 {
            return false;
        }

        // Check collision. New account address is precompile.
        if is_precompile {
            return false;
        }
        acc.storage_cleared = true;

        // Set all storages to default value. They need to be present to act as accessed slots in access list.
        // it shouldn't be possible for them to have different values then zero as code is not existing for this account
        // , but because tests can change that assumption we are doing it.
        let empty = StorageSlot::default();
        acc.storage
            .iter_mut()
            .for_each(|(_, slot)| *slot = empty.clone());

        acc.info.code_hash = KECCAK_EMPTY;
        acc.info.code = None;

        self.journal
            .last_mut()
            .unwrap()
            .push(JournalEntry::AccountTouched { address });
        true
    }

    fn journal_revert(state: &mut State, journal_entries: Vec<JournalEntry>) {
        for entry in journal_entries.into_iter().rev() {
            match entry {
                JournalEntry::AccountLoaded { address } => {
                    state.remove(&address);
                }
                JournalEntry::AccountTouched { address } => {
                    const PRECOMPILE3: H160 =
                        H160([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 3]);

                    if address != PRECOMPILE3 {
                        state.get_mut(&address).unwrap().is_touched = false;
                    }
                }
                JournalEntry::AccountDestroyed {
                    address,
                    target,
                    was_destroyed,
                    had_balance,
                } => {
                    let account = state.get_mut(&address).unwrap();
                    account.is_destroyed = was_destroyed;
                    account.info.balance += had_balance;

                    let target = state.get_mut(&target).unwrap();
                    target.info.balance -= had_balance;
                }
                JournalEntry::BalanceTransfer { from, to, balance } => {
                    // we dont need to check overflow and underflow when adding sub subtracting the balance.
                    let from = state.get_mut(&from).unwrap();
                    from.info.balance += balance;
                    let to = state.get_mut(&to).unwrap();
                    to.info.balance -= balance;
                }
                JournalEntry::NonceChange { address } => {
                    state.get_mut(&address).unwrap().info.nonce -= 1;
                }
                JournalEntry::StorageChage {
                    address,
                    key,
                    had_value,
                } => {
                    let storage = &mut state.get_mut(&address).unwrap().storage;
                    if let Some(had_value) = had_value {
                        storage.get_mut(&key).unwrap().present_value = had_value;
                    } else {
                        storage.remove(&key);
                    }
                }
                JournalEntry::CodeChange { address, had_code } => {
                    let acc = state.get_mut(&address).unwrap();
                    acc.info.code_hash = had_code.hash();
                    acc.info.code = Some(had_code);
                }
            }
        }
    }

    pub fn checkpoint(&mut self) -> JournalCheckpoint {
        let checkpoint = JournalCheckpoint {
            log_i: self.logs.len(),
            journal_i: self.journal.len(),
        };
        self.depth += 1;
        self.journal.push(Default::default());
        checkpoint
    }

    pub fn checkpoint_commit(&mut self) {
        self.depth -= 1;
    }

    pub fn checkpoint_revert(&mut self, checkpoint: JournalCheckpoint) {
        let state = &mut self.state;
        self.depth -= 1;
        // iterate over last N journals sets and revert our global state
        let leng = self.journal.len();
        self.journal
            .iter_mut()
            .rev()
            .take(leng - checkpoint.journal_i)
            .for_each(|cs| Self::journal_revert(state, mem::take(cs)));

        self.logs.truncate(checkpoint.log_i);
        self.journal.truncate(checkpoint.journal_i);
    }

    /// transfer balance from address to target. Check if target exist/is_cold
    pub fn selfdestruct<DB: Database>(
        &mut self,
        address: H160,
        target: H160,
        db: &mut DB,
    ) -> SelfDestructResult {
        let (is_cold, exists) = self.load_account_exist(target, db);
        // transfer all the balance
        let acc = self.state.get_mut(&address).unwrap();
        let balance = mem::take(&mut acc.info.balance);
        let previously_destroyed = acc.is_destroyed;
        acc.is_destroyed = true;
        // In case that target and destroyed addresses are same, balance will be lost.
        // ref: https://github.com/ethereum/go-ethereum/blob/141cd425310b503c5678e674a8c3872cf46b7086/core/vm/instructions.go#L832-L833
        // https://github.com/ethereum/go-ethereum/blob/141cd425310b503c5678e674a8c3872cf46b7086/core/state/statedb.go#L449
        if address != target {
            let target_account = self.state.get_mut(&target).unwrap();
            Self::touch_account(self.journal.last_mut().unwrap(), &target, target_account);
            target_account.info.balance += balance;
        }

        self.journal
            .last_mut()
            .unwrap()
            .push(JournalEntry::AccountDestroyed {
                address,
                target,
                was_destroyed: previously_destroyed,
                had_balance: balance,
            });

        SelfDestructResult {
            had_value: !balance.is_zero(),
            is_cold,
            exists,
            previously_destroyed,
        }
    }

    /// load account into memory. return if it is cold or hot accessed
    pub fn load_account<DB: Database>(&mut self, address: H160, db: &mut DB) -> bool {
        match self.state.entry(address) {
            Entry::Occupied(ref mut _entry) => false,
            Entry::Vacant(vac) => {
                let acc: Account = db.basic(address).into();
                // journal loading of account. AccessList touch.
                self.journal
                    .last_mut()
                    .unwrap()
                    .push(JournalEntry::AccountLoaded { address });

                vac.insert(acc);
                true
            }
        }
    }

    // first is is_cold second bool is exists.
    pub fn load_account_exist<DB: Database>(&mut self, address: H160, db: &mut DB) -> (bool, bool) {
        let (acc, is_cold) = self.load_code(address, db);
        if acc.is_existing_precompile {
            (false, true)
        } else {
            let exists = !acc.is_empty();
            (is_cold, exists)
        }
    }

    pub fn load_code<DB: Database>(&mut self, address: H160, db: &mut DB) -> (&mut Account, bool) {
        let is_cold = self.load_account(address, db);
        let acc = self.state.get_mut(&address).unwrap();
        if acc.info.code.is_none() {
            if acc.info.code_hash == KECCAK_EMPTY {
                let empty = Bytecode::new();
                acc.info.code = Some(empty);
            } else {
                let code = db.code_by_hash(acc.info.code_hash);
                acc.info.code = Some(code);
            }
        }
        (acc, is_cold)
    }

    // account is already present and loaded.
    pub fn sload<DB: Database>(&mut self, address: H160, key: U256, db: &mut DB) -> (U256, bool) {
        let account = self.state.get_mut(&address).unwrap(); // asume acc is hot
        let load = match account.storage.entry(key) {
            Entry::Occupied(occ) => (occ.get().present_value, false),
            Entry::Vacant(vac) => {
                // if storage was cleared, we dont need to ping db.
                let value = if account.storage_cleared {
                    U256::zero()
                } else {
                    db.storage(address, key)
                };
                // add it to journal as cold loaded.
                self.journal
                    .last_mut()
                    .unwrap()
                    .push(JournalEntry::StorageChage {
                        address,
                        key,
                        had_value: None,
                    });

                vac.insert(StorageSlot::new(value));

                (value, true)
            }
        };
        load
    }

    /// account should already be present in our state.
    /// returns (original,present,new) slot
    pub fn sstore<DB: Database>(
        &mut self,
        address: H160,
        key: U256,
        new: U256,
        db: &mut DB,
    ) -> (U256, U256, U256, bool) {
        // assume that acc exists and load the slot.
        let (present, is_cold) = self.sload(address, key, db);
        let acc = self.state.get_mut(&address).unwrap();

        // if there is no original value in dirty return present value, that is our original.
        let slot = acc.storage.get_mut(&key).unwrap();

        // new value is same as present, we dont need to do anything
        if present == new {
            return (slot.original_value, present, new, is_cold);
        }

        self.journal
            .last_mut()
            .unwrap()
            .push(JournalEntry::StorageChage {
                address,
                key,
                had_value: Some(present),
            });
        // insert value into present state.
        slot.present_value = new;
        (slot.original_value, present, new, is_cold)
    }

    /// push log into subroutine
    pub fn log(&mut self, log: Log) {
        self.logs.push(log);
    }
}