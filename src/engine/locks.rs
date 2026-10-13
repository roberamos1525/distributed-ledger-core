//! Per-account locking with deterministic ordering.
//!
//! Every command that touches accounts locks *all* of them before validating
//! and committing, always in ascending [`AccountId`] order. Because every
//! thread acquires locks in the same global order, two commands can never
//! wait on each other in a cycle, so deadlocks are impossible regardless of
//! how postings are listed in a request.
//!
//! Global lock order used by the engine:
//! 1. commit gate (shared; exclusive only for consistent snapshots)
//! 2. at most one transaction lock (pending post/void, reversal)
//! 3. account locks in ascending id order
//! 4. the WAL mutex
//!
//! Index maps (`RwLock<BTreeMap<..>>`) are only held for short lookups and
//! never while waiting for one of the locks above, except `open_account`,
//! which holds the account index write lock across the WAL append; it holds no
//! account locks, so the ordering above is preserved.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::lock_api::ArcMutexGuard;
use parking_lot::{Mutex, RawMutex, RwLock};

use crate::domain::account::AccountState;
use crate::domain::ids::AccountId;
use crate::error::{LedgerError, LedgerResult};

/// Shared, individually lockable account state.
pub type AccountCell = Arc<Mutex<AccountState>>;

/// Owned guard over one account.
pub type AccountGuard = ArcMutexGuard<RawMutex, AccountState>;

/// A set of accounts locked in deterministic order.
pub struct LockedAccounts {
    guards: Vec<(AccountId, AccountGuard)>,
    waited: Duration,
}

impl LockedAccounts {
    /// An empty lock set (for commands that create accounts).
    pub fn empty() -> Self {
        LockedAccounts {
            guards: Vec::new(),
            waited: Duration::ZERO,
        }
    }

    /// Locks the given accounts, sorted and de-duplicated.
    ///
    /// Fails with `AccountNotFound` before taking any lock if an id is unknown.
    pub fn acquire(
        index: &RwLock<BTreeMap<AccountId, AccountCell>>,
        ids: impl IntoIterator<Item = AccountId>,
    ) -> LedgerResult<Self> {
        let ordered: BTreeSet<AccountId> = ids.into_iter().collect();
        let cells: Vec<(AccountId, AccountCell)> = {
            let map = index.read();
            ordered
                .into_iter()
                .map(|id| match map.get(&id) {
                    Some(cell) => Ok((id, cell.clone())),
                    None => Err(LedgerError::AccountNotFound(id)),
                })
                .collect::<LedgerResult<_>>()?
        };
        let started = Instant::now();
        let guards = cells
            .into_iter()
            .map(|(id, cell)| (id, cell.lock_arc()))
            .collect();
        Ok(LockedAccounts {
            guards,
            waited: started.elapsed(),
        })
    }

    /// Time spent waiting for the locks.
    pub fn waited(&self) -> Duration {
        self.waited
    }

    fn position(&self, id: &AccountId) -> Option<usize> {
        self.guards.binary_search_by(|(k, _)| k.cmp(id)).ok()
    }

    pub fn get(&self, id: &AccountId) -> Option<&AccountState> {
        self.position(id).map(|i| &*self.guards[i].1)
    }

    pub fn get_mut(&mut self, id: &AccountId) -> Option<&mut AccountState> {
        self.position(id).map(|i| &mut *self.guards[i].1)
    }

    /// Like [`get`](Self::get) but returns `AccountNotFound` for unknown ids.
    pub fn require(&self, id: &AccountId) -> LedgerResult<&AccountState> {
        self.get(id)
            .ok_or_else(|| LedgerError::AccountNotFound(id.clone()))
    }

    pub fn ids(&self) -> impl Iterator<Item = &AccountId> {
        self.guards.iter().map(|(id, _)| id)
    }

    pub fn len(&self) -> usize {
        self.guards.len()
    }

    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::account::{Account, AccountStatus, AccountType, BalancePolicy};
    use crate::domain::money::CurrencyCode;
    use chrono::Utc;

    fn index(ids: &[&str]) -> RwLock<BTreeMap<AccountId, AccountCell>> {
        let now = Utc::now();
        let map = ids
            .iter()
            .map(|id| {
                let id = AccountId::new(*id).unwrap();
                let state = AccountState::new(Account {
                    id: id.clone(),
                    name: id.to_string(),
                    account_type: AccountType::Asset,
                    currency: CurrencyCode::new("USD").unwrap(),
                    status: AccountStatus::Open,
                    policy: BalancePolicy::Unrestricted,
                    metadata: Default::default(),
                    version: 1,
                    created_at: now,
                    created_sequence: 1,
                    updated_at: now,
                });
                (id, Arc::new(Mutex::new(state)))
            })
            .collect();
        RwLock::new(map)
    }

    fn ids(raw: &[&str]) -> Vec<AccountId> {
        raw.iter().map(|r| AccountId::new(*r).unwrap()).collect()
    }

    #[test]
    fn locks_sorted_and_deduplicated() {
        let idx = index(&["a", "b", "c"]);
        let locked = LockedAccounts::acquire(&idx, ids(&["c", "a", "c"])).unwrap();
        let order: Vec<_> = locked.ids().map(|i| i.to_string()).collect();
        assert_eq!(order, vec!["a", "c"]);
        assert_eq!(locked.len(), 2);
        assert!(locked.get(&AccountId::new("b").unwrap()).is_none());
        assert!(locked.require(&AccountId::new("b").unwrap()).is_err());
    }

    #[test]
    fn unknown_account_fails_without_locking() {
        let idx = index(&["a"]);
        let err = LockedAccounts::acquire(&idx, ids(&["a", "zzz"]))
            .err()
            .unwrap();
        assert_eq!(err.code(), "account_not_found");
        // "a" must not be left locked.
        assert!(
            idx.read()[&AccountId::new("a").unwrap()]
                .try_lock()
                .is_some()
        );
    }

    #[test]
    fn guards_release_on_drop_and_allow_mutation() {
        let idx = index(&["a", "b"]);
        {
            let mut locked = LockedAccounts::acquire(&idx, ids(&["b", "a"])).unwrap();
            locked
                .get_mut(&AccountId::new("a").unwrap())
                .unwrap()
                .balances
                .posted = 7;
            assert!(
                idx.read()[&AccountId::new("a").unwrap()]
                    .try_lock()
                    .is_none()
            );
        }
        let cell = idx.read()[&AccountId::new("a").unwrap()].clone();
        assert_eq!(cell.lock().balances.posted, 7);
    }

    #[test]
    fn opposite_orders_do_not_deadlock() {
        let idx = Arc::new(index(&["x", "y", "z"]));
        std::thread::scope(|s| {
            for t in 0..8 {
                let idx = idx.clone();
                s.spawn(move || {
                    let order = if t % 2 == 0 {
                        ids(&["x", "y", "z"])
                    } else {
                        ids(&["z", "y", "x"])
                    };
                    for _ in 0..500 {
                        let mut locked = LockedAccounts::acquire(&idx, order.clone()).unwrap();
                        for id in order.iter() {
                            locked.get_mut(id).unwrap().balances.posted += 1;
                        }
                    }
                });
            }
        });
        let total: i64 = idx.read().values().map(|c| c.lock().balances.posted).sum();
        assert_eq!(total, 8 * 500 * 3);
    }
}
