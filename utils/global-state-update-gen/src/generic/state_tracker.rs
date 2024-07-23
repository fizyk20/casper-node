use std::{
    cmp::Ordering,
    collections::{btree_map::Entry, BTreeMap, BTreeSet},
    convert::TryFrom,
};

use rand::Rng;

use casper_types::{
    account::{Account, AccountHash},
    system::auction::{Bid, Bids, SeigniorageRecipientsSnapshot, UnbondingPurse},
    AccessRights, CLValue, Key, PublicKey, StoredValue, URef, U512,
};

use super::{config::Transfer, state_reader::StateReader};

/// A struct tracking changes to be made to the global state.
pub struct StateTracker<T> {
    reader: T,
    entries_to_write: BTreeMap<Key, StoredValue>,
    total_supply: U512,
    total_supply_key: Key,
    accounts_cache: BTreeMap<AccountHash, Account>,
    unbonds_cache: BTreeMap<AccountHash, Vec<UnbondingPurse>>,
    purses_cache: BTreeMap<URef, U512>,
    bids_cache: Option<Bids>,
    seigniorage_recipients: Option<(Key, SeigniorageRecipientsSnapshot)>,
}

impl<T: StateReader> StateTracker<T> {
    /// Creates a new `StateTracker`.
    pub fn new(mut reader: T) -> Self {
        // Read the URef under which total supply is stored.
        let total_supply_key = reader.get_total_supply_key();

        // Read the total supply.
        let total_supply_sv = reader.query(total_supply_key).expect("should query");
        let total_supply = total_supply_sv
            .as_cl_value()
            .cloned()
            .expect("should be cl value");

        Self {
            reader,
            entries_to_write: Default::default(),
            total_supply_key,
            total_supply: total_supply.into_t().expect("should be U512"),
            accounts_cache: BTreeMap::new(),
            unbonds_cache: BTreeMap::new(),
            purses_cache: BTreeMap::new(),
            bids_cache: None,
            seigniorage_recipients: None,
        }
    }

    /// Returns all the entries to be written to the global state
    pub fn get_entries(&self) -> BTreeMap<Key, StoredValue> {
        self.entries_to_write.clone()
    }

    /// Stores a write of an entry in the global state.
    pub fn write_entry(&mut self, key: Key, value: StoredValue) {
        let _ = self.entries_to_write.insert(key, value);
    }

    /// Increases the total supply of the tokens in the network.
    pub fn increase_supply(&mut self, to_add: U512) {
        self.total_supply += to_add;
        self.write_entry(
            self.total_supply_key,
            StoredValue::CLValue(CLValue::from_t(self.total_supply).unwrap()),
        );
    }

    /// Decreases the total supply of the tokens in the network.
    pub fn decrease_supply(&mut self, to_sub: U512) {
        self.total_supply -= to_sub;
        self.write_entry(
            self.total_supply_key,
            StoredValue::CLValue(CLValue::from_t(self.total_supply).unwrap()),
        );
    }

    /// Creates a new purse containing the given amount of motes and returns its URef.
    pub fn create_purse(&mut self, amount: U512) -> URef {
        let mut rng = rand::thread_rng();
        let new_purse = URef::new(rng.gen(), AccessRights::READ_ADD_WRITE);

        // Purse URef pointing to `()` so that the owner cannot modify the purse directly.
        self.write_entry(Key::URef(new_purse), StoredValue::CLValue(CLValue::unit()));

        self.set_purse_balance(new_purse, amount);

        new_purse
    }

    /// Gets the balance of the purse, taking into account changes made during the update.
    pub fn get_purse_balance(&mut self, purse: URef) -> U512 {
        match self.purses_cache.get(&purse).cloned() {
            Some(amount) => amount,
            None => {
                let base_key = Key::Balance(purse.addr());
                let amount = self
                    .reader
                    .query(base_key)
                    .map(|v| CLValue::try_from(v).expect("purse balance should be a CLValue"))
                    .map(|cl_value| cl_value.into_t().expect("purse balance should be a U512"))
                    .unwrap_or_else(U512::zero);
                self.purses_cache.insert(purse, amount);
                amount
            }
        }
    }

    /// Sets the balance of the purse.
    pub fn set_purse_balance(&mut self, purse: URef, balance: U512) {
        let current_balance = self.get_purse_balance(purse);

        match balance.cmp(&current_balance) {
            Ordering::Greater => self.increase_supply(balance - current_balance),
            Ordering::Less => self.decrease_supply(current_balance - balance),
            Ordering::Equal => return,
        }

        self.write_entry(
            Key::Balance(purse.addr()),
            StoredValue::CLValue(CLValue::from_t(balance).unwrap()),
        );
        self.purses_cache.insert(purse, balance);
    }

    /// Creates a new account for the given public key and seeds it with the given amount of
    /// tokens.
    pub fn create_account(&mut self, account_hash: AccountHash, amount: U512) -> Account {
        let main_purse = self.create_purse(amount);

        let account = Account::create(account_hash, Default::default(), main_purse);

        self.accounts_cache.insert(account_hash, account.clone());

        self.write_entry(
            Key::Account(account_hash),
            StoredValue::Account(account.clone()),
        );

        account
    }

    /// Gets the account for the given public key.
    pub fn get_account(&mut self, account_hash: &AccountHash) -> Option<Account> {
        match self.accounts_cache.entry(*account_hash) {
            Entry::Vacant(vac) => self
                .reader
                .get_account(*account_hash)
                .map(|account| vac.insert(account).clone()),
            Entry::Occupied(occupied) => Some(occupied.into_mut().clone()),
        }
    }

    pub fn execute_transfer(&mut self, transfer: &Transfer) {
        let from_account = if let Some(account) = self.get_account(&transfer.from) {
            account
        } else {
            eprintln!("\"from\" account doesn't exist; transfer: {:?}", transfer);
            return;
        };

        let to_account = if let Some(account) = self.get_account(&transfer.to) {
            account
        } else {
            self.create_account(transfer.to, U512::zero())
        };

        let from_balance = self.get_purse_balance(from_account.main_purse());

        if from_balance < transfer.amount {
            eprintln!(
                "\"from\" account balance insufficient; balance = {}, transfer = {:?}",
                from_balance, transfer
            );
            return;
        }

        let to_balance = self.get_purse_balance(to_account.main_purse());

        self.set_purse_balance(from_account.main_purse(), from_balance - transfer.amount);
        self.set_purse_balance(to_account.main_purse(), to_balance + transfer.amount);
    }

    /// Reads the `SeigniorageRecipientsSnapshot` stored in the global state.
    pub fn read_snapshot(&mut self) -> (Key, SeigniorageRecipientsSnapshot) {
        if let Some(key_and_snapshot) = &self.seigniorage_recipients {
            return key_and_snapshot.clone();
        }

        // Read the key under which the snapshot is stored.
        let validators_key = self.reader.get_seigniorage_recipients_key();

        // Decode the old snapshot.
        let stored_value = self.reader.query(validators_key).expect("should query");
        let cl_value = stored_value
            .as_cl_value()
            .cloned()
            .expect("should be cl value");
        let snapshot: SeigniorageRecipientsSnapshot = cl_value.into_t().expect("should convert");
        self.seigniorage_recipients = Some((validators_key, snapshot.clone()));
        (validators_key, snapshot)
    }

    /// Reads the bids from the global state.
    pub fn get_bids(&mut self) -> Bids {
        if let Some(ref bids) = self.bids_cache {
            bids.clone()
        } else {
            let bids = self.reader.get_bids();
            self.bids_cache = Some(bids.clone());
            bids
        }
    }

    /// Sets the bid for the given account.
    pub fn set_bid(&mut self, public_key: PublicKey, bid: Bid, slash: bool) {
        let maybe_current_bid = self.get_bids().get(&public_key).cloned();

        let account_hash = public_key.to_account_hash();
        let already_unbonded = self.already_unbonding_amount(&public_key);

        // we need to put enough funds in the bonding purse to cover the staked amount and the
        // amount that will be getting unbonded in the future
        let new_amount = *bid.staked_amount() + already_unbonded;
        let old_amount = maybe_current_bid
            .as_ref()
            .map(|bid| self.get_purse_balance(*bid.bonding_purse()))
            .unwrap_or_else(U512::zero);

        // we called `get_bids` above, so `bids_cache` will be `Some`
        self.bids_cache
            .as_mut()
            .unwrap()
            .insert(public_key.clone(), bid.clone());

        // Replace the bid (overwrite the previous bid, if any):
        self.write_entry(Key::Bid(account_hash), bid.clone().into());

        // Update the bonding purses - this will also take care of the total supply changes.
        if let Some(old_bid) = maybe_current_bid {
            // only zero the bonding purse and write the balance to the new purse if the new one is
            // different (just to save unnecessary writes)
            if old_bid.bonding_purse() != bid.bonding_purse() {
                self.set_purse_balance(*old_bid.bonding_purse(), U512::zero());
                self.set_purse_balance(*bid.bonding_purse(), old_amount);
                // we zeroed out the bonding purse - remove any unbonds that could be trying to
                // transfer funds out of it
                self.remove_withdraws_and_unbonds_single(&public_key, &public_key);
            }

            for (delegator_pub_key, delegator) in old_bid
                .delegators()
                .iter()
                // Skip zeroing purses of delegators whose purses do not change - that will
                // be handled later:
                .filter(|(delegator_pub_key, delegator)| {
                    bid.delegators()
                        .get(delegator_pub_key)
                        .map_or(true, |new_delegator| {
                            new_delegator.bonding_purse() != delegator.bonding_purse()
                        })
                })
            {
                // zero the old purse if we are going to transfer the funds to a new purse, or if
                // we're supposed to slash the delegator
                if slash || bid.delegators().contains_key(delegator_pub_key) {
                    // if we're transferring funds to a new purse, set the new purse balance to the
                    // old amount
                    let old_amount = self.get_purse_balance(*delegator.bonding_purse());
                    if let Some(new_delegator) = bid.delegators().get(delegator_pub_key) {
                        self.set_purse_balance(*new_delegator.bonding_purse(), old_amount);
                    }
                    self.set_purse_balance(*delegator.bonding_purse(), U512::zero());
                    // we zeroed out the bonding purse - remove any unbonds that could be trying to
                    // transfer funds out of it
                    self.remove_withdraws_and_unbonds_single(&public_key, delegator_pub_key);
                } else {
                    let amount = self.get_purse_balance(*delegator.bonding_purse());
                    let already_unbonding = self.already_unbonding_amount(delegator_pub_key);
                    self.create_unbonding_purse(
                        *delegator.bonding_purse(),
                        &public_key,
                        delegator_pub_key,
                        amount - already_unbonding,
                    );
                }
            }
        }

        if (slash && new_amount != old_amount) || new_amount > old_amount {
            if slash {
                // we're resetting the bonded amount, remove the unbonding entries so that they
                // don't cause inconsistencies
                self.remove_withdraws_and_unbonds_single(&public_key, &public_key);
            }
            self.set_purse_balance(*bid.bonding_purse(), new_amount);
        } else if new_amount < old_amount {
            self.create_unbonding_purse(
                *bid.bonding_purse(),
                &public_key,
                &public_key,
                old_amount - new_amount,
            );
        }

        for (delegator_public_key, delegator) in bid.delegators() {
            let old_amount = self.get_purse_balance(*delegator.bonding_purse());
            let already_unbonded = self.already_unbonding_amount(delegator_public_key);
            let new_amount = *delegator.staked_amount() + already_unbonded;
            if (slash && new_amount != old_amount) || new_amount > old_amount {
                if slash {
                    // we're resetting the bonded amount, remove the unbonding entries so that they
                    // don't cause inconsistencies
                    self.remove_withdraws_and_unbonds_single(&public_key, delegator_public_key);
                }
                self.set_purse_balance(*delegator.bonding_purse(), new_amount);
            } else if new_amount < old_amount {
                self.create_unbonding_purse(
                    *delegator.bonding_purse(),
                    &public_key,
                    delegator_public_key,
                    old_amount - new_amount,
                );
            }
        }
    }

    /// Returns the sum of already unbonding purses for the given validator account & unbonder.
    fn already_unbonding_amount(&mut self, unbonder: &PublicKey) -> U512 {
        let account = unbonder.to_account_hash();
        let current_era = *self.read_snapshot().1.keys().next().unwrap();
        let unbonding_delay = self.reader.get_unbonding_delay();
        let limit_era = current_era.saturating_sub(unbonding_delay);

        let existing_purses = self
            .reader
            .get_unbonds()
            .get(&account)
            .cloned()
            .unwrap_or_default();
        let existing_purses_legacy = self
            .reader
            .get_withdraws()
            .get(&account)
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .map(UnbondingPurse::from)
            // There may be some leftover old legacy purses that haven't been purged - this is to
            // make sure that we don't accidentally take them into account.
            .filter(|purse| purse.era_of_creation() >= limit_era);

        existing_purses_legacy
            .chain(existing_purses)
            .filter(|purse| purse.unbonder_public_key() == unbonder)
            .map(|purse| *purse.amount())
            .sum()
    }

    pub fn remove_withdraws_and_unbonds_single(
        &mut self,
        validator: &PublicKey,
        unbonder: &PublicKey,
    ) {
        let withdraws = self.reader.get_withdraws();
        let unbonds = self.reader.get_unbonds();

        let unbonder_account = unbonder.to_account_hash();
        if let Some(unbonds) = unbonds.get(&unbonder_account) {
            let mut new_unbonds = unbonds.clone();
            new_unbonds.retain(|unbonding_purse| {
                if unbonding_purse.validator_public_key() == validator
                    && unbonding_purse.unbonder_public_key() == unbonder
                {
                    self.set_purse_balance(*unbonding_purse.bonding_purse(), U512::zero());
                    false
                } else {
                    true
                }
            });
            if &new_unbonds != unbonds {
                self.write_entry(
                    Key::Unbond(unbonder_account),
                    StoredValue::Unbonding(new_unbonds),
                );
            }
        }

        if let Some(withdraws) = withdraws.get(&unbonder_account) {
            let mut new_withdraws = withdraws.clone();
            new_withdraws.retain(|withdraw| {
                if withdraw.validator_public_key() == validator
                    && withdraw.unbonder_public_key() == unbonder
                {
                    self.set_purse_balance(*withdraw.bonding_purse(), U512::zero());
                    false
                } else {
                    true
                }
            });
            if &new_withdraws != withdraws {
                self.write_entry(
                    Key::Withdraw(unbonder_account),
                    StoredValue::Withdraw(new_withdraws),
                );
            }
        }
    }

    /// Generates the writes to the global state that will remove the pending withdraws and unbonds
    /// of all the old validators that will cease to be validators, and slashes their unbonding
    /// purses.
    pub fn remove_withdraws_and_unbonds(&mut self, removed: &BTreeSet<PublicKey>) {
        let withdraws = self.reader.get_withdraws();
        let unbonds = self.reader.get_unbonds();

        for (unbonder_key, unbonding_purses) in &unbonds {
            let mut new_unbonding_purses = unbonding_purses.clone();
            new_unbonding_purses.retain(|unbonding_purse| {
                if removed.contains(unbonding_purse.validator_public_key()) {
                    self.set_purse_balance(*unbonding_purse.bonding_purse(), U512::zero());
                    false
                } else {
                    true
                }
            });
            if &new_unbonding_purses != unbonding_purses {
                self.write_entry(
                    Key::Unbond(*unbonder_key),
                    StoredValue::Unbonding(new_unbonding_purses),
                );
            }
        }

        for (withdrawer_key, withdraw_entries) in &withdraws {
            let mut new_withdraw_entries = withdraw_entries.clone();
            new_withdraw_entries.retain(|withdraw| {
                if removed.contains(withdraw.validator_public_key()) {
                    self.set_purse_balance(*withdraw.bonding_purse(), U512::zero());
                    false
                } else {
                    true
                }
            });
            if &new_withdraw_entries != withdraw_entries {
                self.write_entry(
                    Key::Withdraw(*withdrawer_key),
                    StoredValue::Withdraw(new_withdraw_entries),
                );
            }
        }
    }

    pub fn create_unbonding_purse(
        &mut self,
        bonding_purse: URef,
        validator_key: &PublicKey,
        unbonder_key: &PublicKey,
        amount: U512,
    ) {
        let account_hash = unbonder_key.to_account_hash();
        let unbonding_era = self.read_snapshot().1.keys().next().copied().unwrap();
        let unbonding_purses = match self.unbonds_cache.entry(account_hash) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                // Fill the cache with the information from the reader when the cache is empty:
                let existing_purses = self
                    .reader
                    .get_unbonds()
                    .get(&account_hash)
                    .cloned()
                    .unwrap_or_default();

                entry.insert(existing_purses)
            }
        };

        // Take the first era from the snapshot as the unbonding era.
        let new_purse = UnbondingPurse::new(
            bonding_purse,
            validator_key.clone(),
            unbonder_key.clone(),
            unbonding_era,
            amount,
            None,
        );

        // This doesn't actually transfer or create any funds - the funds will be transferred from
        // the bonding purse to the unbonder's main purse later by the auction contract.
        unbonding_purses.push(new_purse);
        let unbonding_purses = unbonding_purses.clone();
        self.write_entry(
            Key::Unbond(account_hash),
            StoredValue::Unbonding(unbonding_purses),
        );
    }
}
