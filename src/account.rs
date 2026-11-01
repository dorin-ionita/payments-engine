//! A single client's asset account and the balance operations on it.
//!
//! The account knows nothing about transactions or disputes; it only moves
//! money between `available` and `held` and enforces the rules that hold for
//! every operation: a locked account rejects everything (assumption 3), and a
//! change is applied completely or not at all.

use crate::amount::Amount;

/// Balances of one client.
///
/// Invariant: `total == available + held`. All three fields are written only
/// by [`Account::update`], which computes `total` itself, so they cannot
/// drift apart.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Account {
    available: Amount,
    held: Amount,
    total: Amount,
    locked: bool,
}

/// Why a balance operation was refused. The account is unchanged in every case.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AccountError {
    #[error("account is locked")]
    Locked,
    #[error("insufficient available funds")]
    InsufficientFunds,
    #[error("insufficient held funds")]
    InsufficientHeld,
    #[error("balance would overflow")]
    Overflow,
}

impl Account {
    pub fn available(&self) -> Amount {
        self.available
    }

    pub fn held(&self) -> Amount {
        self.held
    }

    pub fn total(&self) -> Amount {
        self.total
    }

    pub fn is_locked(&self) -> bool {
        self.locked
    }

    /// Credits `amount` to the available funds.
    pub fn deposit(&mut self, amount: Amount) -> Result<(), AccountError> {
        self.ensure_unlocked()?;
        self.update(self.available.checked_add(amount), Some(self.held))
    }

    /// Debits `amount` from the available funds. Held funds can't be withdrawn.
    pub fn withdraw(&mut self, amount: Amount) -> Result<(), AccountError> {
        self.ensure_unlocked()?;
        if self.available < amount {
            return Err(AccountError::InsufficientFunds);
        }
        self.update(self.available.checked_sub(amount), Some(self.held))
    }

    /// Moves `amount` from available to held; the total is unchanged.
    ///
    /// `available` may become negative here (assumption 4): the caller decides
    /// whether that is acceptable before calling.
    pub fn hold(&mut self, amount: Amount) -> Result<(), AccountError> {
        self.ensure_unlocked()?;
        self.update(
            self.available.checked_sub(amount),
            self.held.checked_add(amount),
        )
    }

    /// Moves `amount` from held back to available; the total is unchanged.
    pub fn release(&mut self, amount: Amount) -> Result<(), AccountError> {
        self.ensure_unlocked()?;
        self.ensure_held(amount)?;
        self.update(
            self.available.checked_add(amount),
            self.held.checked_sub(amount),
        )
    }

    /// Removes `amount` from held funds (so the total drops) and locks the account.
    pub fn charge_back(&mut self, amount: Amount) -> Result<(), AccountError> {
        self.ensure_unlocked()?;
        self.ensure_held(amount)?;
        self.update(Some(self.available), self.held.checked_sub(amount))?;
        self.locked = true;
        Ok(())
    }

    fn ensure_unlocked(&self) -> Result<(), AccountError> {
        if self.locked {
            return Err(AccountError::Locked);
        }
        Ok(())
    }

    fn ensure_held(&self, amount: Amount) -> Result<(), AccountError> {
        if self.held < amount {
            return Err(AccountError::InsufficientHeld);
        }
        Ok(())
    }

    /// The only place balances are written. `None` means an earlier checked
    /// operation overflowed; in that case, or if the new total would overflow,
    /// nothing changes.
    fn update(
        &mut self,
        available: Option<Amount>,
        held: Option<Amount>,
    ) -> Result<(), AccountError> {
        let available = available.ok_or(AccountError::Overflow)?;
        let held = held.ok_or(AccountError::Overflow)?;
        let total = available.checked_add(held).ok_or(AccountError::Overflow)?;
        self.available = available;
        self.held = held;
        self.total = total;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn amt(s: &str) -> Amount {
        s.parse().unwrap()
    }

    fn account_with(available: &str) -> Account {
        let mut account = Account::default();
        account.deposit(amt(available)).unwrap();
        account
    }

    fn assert_balances(account: &Account, available: &str, held: &str, total: &str) {
        assert_eq!(account.available(), amt(available), "available");
        assert_eq!(account.held(), amt(held), "held");
        assert_eq!(account.total(), amt(total), "total");
    }

    #[test]
    fn new_account_is_empty_and_unlocked() {
        let account = Account::default();
        assert_balances(&account, "0", "0", "0");
        assert!(!account.is_locked());
    }

    #[test]
    fn deposit_increases_available_and_total() {
        let mut account = account_with("1.5");
        account.deposit(amt("2.25")).unwrap();
        assert_balances(&account, "3.75", "0", "3.75");
    }

    #[test]
    fn withdraw_decreases_available_and_total() {
        let mut account = account_with("3");
        account.withdraw(amt("1.25")).unwrap();
        assert_balances(&account, "1.75", "0", "1.75");
    }

    #[test]
    fn withdraw_of_exact_balance_succeeds() {
        let mut account = account_with("3");
        account.withdraw(amt("3")).unwrap();
        assert_balances(&account, "0", "0", "0");
    }

    #[test]
    fn withdraw_with_insufficient_funds_changes_nothing() {
        let mut account = account_with("1");
        let before = account.clone();
        assert_eq!(
            account.withdraw(amt("1.0001")),
            Err(AccountError::InsufficientFunds)
        );
        assert_eq!(account, before);
    }

    #[test]
    fn held_funds_cannot_be_withdrawn() {
        let mut account = account_with("5");
        account.hold(amt("4")).unwrap();
        assert_eq!(
            account.withdraw(amt("2")),
            Err(AccountError::InsufficientFunds)
        );
        assert_balances(&account, "1", "4", "5");
    }

    #[test]
    fn hold_moves_funds_from_available_to_held() {
        let mut account = account_with("5");
        account.hold(amt("2")).unwrap();
        assert_balances(&account, "3", "2", "5");
    }

    #[test]
    fn hold_may_drive_available_negative() {
        let mut account = account_with("5");
        account.withdraw(amt("4")).unwrap();
        account.hold(amt("5")).unwrap();
        assert_balances(&account, "-4", "5", "1");
    }

    #[test]
    fn release_moves_funds_back_to_available() {
        let mut account = account_with("5");
        account.hold(amt("2")).unwrap();
        account.release(amt("2")).unwrap();
        assert_balances(&account, "5", "0", "5");
    }

    #[test]
    fn release_more_than_held_changes_nothing() {
        let mut account = account_with("5");
        account.hold(amt("1")).unwrap();
        let before = account.clone();
        assert_eq!(
            account.release(amt("2")),
            Err(AccountError::InsufficientHeld)
        );
        assert_eq!(account, before);
    }

    #[test]
    fn charge_back_removes_held_funds_and_locks() {
        let mut account = account_with("5");
        account.hold(amt("2")).unwrap();
        account.charge_back(amt("2")).unwrap();
        assert_balances(&account, "3", "0", "3");
        assert!(account.is_locked());
    }

    #[test]
    fn charge_back_more_than_held_changes_nothing() {
        let mut account = account_with("5");
        let before = account.clone();
        assert_eq!(
            account.charge_back(amt("1")),
            Err(AccountError::InsufficientHeld)
        );
        assert_eq!(account, before);
    }

    #[test]
    fn locked_account_rejects_every_operation() {
        let mut account = account_with("5");
        account.hold(amt("3")).unwrap();
        account.charge_back(amt("1")).unwrap();
        let before = account.clone();

        assert_eq!(account.deposit(amt("1")), Err(AccountError::Locked));
        assert_eq!(account.withdraw(amt("1")), Err(AccountError::Locked));
        assert_eq!(account.hold(amt("1")), Err(AccountError::Locked));
        assert_eq!(account.release(amt("1")), Err(AccountError::Locked));
        assert_eq!(account.charge_back(amt("1")), Err(AccountError::Locked));
        assert_eq!(account, before);
    }

    #[test]
    fn overflowing_deposit_changes_nothing() {
        let mut account = Account::default();
        account.deposit(Amount::from_units(i64::MAX)).unwrap();
        let before = account.clone();
        assert_eq!(account.deposit(amt("0.0001")), Err(AccountError::Overflow));
        assert_eq!(account, before);
    }

    #[test]
    fn overflowing_total_changes_nothing() {
        // available and held each fit, but their sum would not.
        let mut account = Account::default();
        account.deposit(Amount::from_units(i64::MAX)).unwrap();
        account.hold(Amount::from_units(i64::MAX)).unwrap();
        let before = account.clone();
        assert_eq!(account.deposit(amt("0.0001")), Err(AccountError::Overflow));
        assert_eq!(account, before);
    }
}
