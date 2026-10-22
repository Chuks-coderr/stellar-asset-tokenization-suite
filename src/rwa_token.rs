use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Env, Map,
    String, Symbol, Vec,
};

use crate::compliance_registry::ComplianceRegistryClient;

const STORAGE_VERSION: u32 = 2;

/// Maximum number of metadata entries a token may carry.
const MAX_METADATA_KEYS: u32 = 50;
/// Maximum length (in bytes) of a metadata value.
const MAX_METADATA_VALUE_LEN: u32 = 256;

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum RWATokenError {
    InsufficientBalance = 1,
    ComplianceCheckFailed = 2,
    TransferPaused = 3,
    Unauthorized = 4,
    InvalidAmount = 5,
    AssetFrozen = 6,
    KYCRequired = 7,
    TransferRestriction = 8,
    AlreadyInitialized = 9,
    NotInitialized = 10,
    TokenInfoNotFound = 11,
    Overflow = 12,
    Underflow = 13,
    StorageOutdated = 14,
    TokenNotInitialized = 15,
    AlreadyAtLatestVersion = 16,
    TokenPaused = 17,
    InsufficientAllowance = 18,
    MetadataKeyImmutable = 19,
    MetadataLimitExceeded = 20,
    MetadataValueTooLong = 21,
}

#[contracttype]
#[derive(Clone)]
pub struct TokenInfo {
    pub name: Symbol,
    pub symbol: Symbol,
    pub total_supply: i128,
    pub decimals: u32,
    pub asset_type: Symbol,
    pub metadata: Map<Symbol, String>,
    pub compliance_registry: Address,
    pub dividend_distributor: Address,
    pub created_at: u64,
    pub is_paused: bool,
    pub is_frozen: bool,
}

#[contracttype]
#[derive(Clone)]
pub struct Balance {
    /// Total holdings, including tokens that are currently locked.
    pub amount: i128,
    /// Portion of `amount` that is currently locked and therefore not spendable.
    pub locked_amount: i128,
    pub voting_power: i128,
    pub last_dividend_claim: u64,
}

#[contracttype]
pub struct TransferRestriction {
    pub max_daily_amount: i128,
    pub max_monthly_amount: i128,
    pub requires_accreditation: bool,
    pub geographic_restrictions: Vec<Symbol>,
}

#[contracttype]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VestingType {
    /// The whole slot unlocks at `until`.
    Cliff = 0,
    /// The slot unlocks linearly between `created_at` and `until`.
    Linear = 1,
}

#[contracttype]
#[derive(Clone)]
pub struct LockSlot {
    /// Original number of tokens in this slot.
    pub amount: i128,
    /// Timestamp at which the slot is fully unlocked.
    pub until: u64,
    /// Timestamp at which the slot was created (start of linear vesting).
    pub created_at: u64,
    pub vesting_type: VestingType,
}

fn locks_key(env: &Env) -> Symbol {
    Symbol::new(env, "locks")
}

fn allowances_key(env: &Env) -> Symbol {
    Symbol::new(env, "allowances")
}

fn initialized_key(env: &Env) -> Symbol {
    Symbol::new(env, "initialized")
}

#[contract]
pub struct RWAToken;

#[contractimpl]
impl RWAToken {
    pub fn initialize(
        env: Env,
        auth: Address,
        name: Symbol,
        symbol: Symbol,
        total_supply: i128,
        decimals: u32,
        asset_type: Symbol,
        metadata: Map<Symbol, String>,
        compliance_registry: Address,
        dividend_distributor: Address,
    ) {
        if env.storage().instance().has(&initialized_key(&env))
            || env
                .storage()
                .instance()
                .has(&Symbol::new(&env, "token_info"))
        {
            panic_with_error!(&env, RWATokenError::AlreadyInitialized);
        }

        if total_supply <= 0 || decimals > 18 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        if metadata.len() > MAX_METADATA_KEYS {
            panic_with_error!(&env, RWATokenError::MetadataLimitExceeded);
        }

        let token_info = TokenInfo {
            name: name.clone(),
            symbol: symbol.clone(),
            // The initial supply is added to the admin balance by `mint`
            // below, which also updates `total_supply`.
            total_supply: 0,
            decimals,
            asset_type: asset_type.clone(),
            metadata,
            compliance_registry,
            dividend_distributor,
            created_at: env.ledger().timestamp(),
            is_paused: false,
            is_frozen: false,
        };

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
        env.storage()
            .instance()
            .set(&locks_key(&env), &Map::<Address, Vec<LockSlot>>::new(&env));
        env.storage().instance().set(
            &allowances_key(&env),
            &Map::<(Address, Address), i128>::new(&env),
        );
        env.storage().instance().set(&initialized_key(&env), &true);

        // The initializing `auth` becomes this token's admin: `mint` (which
        // performs the single auth check for this frame) requires it.
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "admin"), &auth);

        Self::mint(env.clone(), auth.clone(), auth.clone(), total_supply);

        env.events().publish(
            (Symbol::new(&env, "token_initialized"), auth),
            (name, symbol, total_supply, decimals, asset_type),
        );
    }

    fn read_version(env: &Env) -> u32 {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "version"))
            .unwrap_or(0)
    }

    fn check_version(env: &Env) {
        if Self::read_version(env) < STORAGE_VERSION {
            panic_with_error!(env, RWATokenError::StorageOutdated);
        }
    }

    fn read_token_info(env: &Env) -> TokenInfo {
        env.storage()
            .instance()
            .get(&Symbol::new(env, "token_info"))
            .unwrap_or_else(|| panic_with_error!(env, RWATokenError::TokenNotInitialized))
    }

    pub fn migrate(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);

        let ver = Self::read_version(&env);
        if ver >= STORAGE_VERSION {
            panic_with_error!(&env, RWATokenError::AlreadyAtLatestVersion);
        }

        // v1 -> v2: lock slots gained `created_at` / `vesting_type` and the
        // allowance table was introduced. Existing locks are interpreted as
        // cliff locks so they behave exactly as before.
        if ver < 2 {
            let old_locks: Map<Address, LockSlot> = env
                .storage()
                .instance()
                .get(&locks_key(&env))
                .unwrap_or_else(|| Map::new(&env));

            let now = env.ledger().timestamp();
            let mut new_locks: Map<Address, Vec<LockSlot>> = Map::new(&env);
            for (owner, old_slot) in old_locks.iter() {
                let mut slots = Vec::new(&env);
                slots.push_back(LockSlot {
                    amount: old_slot.amount,
                    until: old_slot.until,
                    created_at: now,
                    vesting_type: VestingType::Cliff,
                });
                new_locks.set(owner, slots);
            }
            env.storage().instance().set(&locks_key(&env), &new_locks);
            env.storage().instance().set(
                &allowances_key(&env),
                &Map::<(Address, Address), i128>::new(&env),
            );
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    // ── balances & lock accounting ────────────────────────────────────────────

    fn read_balance_raw(env: &Env, address: &Address) -> Balance {
        env.storage().instance().get(address).unwrap_or(Balance {
            amount: 0,
            locked_amount: 0,
            voting_power: 0,
            last_dividend_claim: 0,
        })
    }

    fn read_locks(env: &Env) -> Map<Address, Vec<LockSlot>> {
        env.storage()
            .instance()
            .get(&locks_key(env))
            .unwrap_or_else(|| Map::new(env))
    }

    /// Amount of `slot` that is still locked at `now`.
    fn slot_locked(slot: &LockSlot, now: u64) -> i128 {
        if now >= slot.until {
            return 0;
        }

        match slot.vesting_type {
            VestingType::Cliff => slot.amount,
            VestingType::Linear => {
                if slot.until <= slot.created_at {
                    return slot.amount;
                }

                let elapsed = now.saturating_sub(slot.created_at) as u128;
                let duration = (slot.until - slot.created_at) as u128;
                let amount = slot.amount.max(0) as u128;

                // vested = amount * elapsed / duration. Multiplication is
                // performed in u128 and saturates to avoid overflow for very
                // large token amounts.
                let vested = amount
                    .checked_mul(elapsed)
                    .unwrap_or(u128::MAX)
                    .checked_div(duration)
                    .unwrap_or(0)
                    .min(amount);

                (amount - vested) as i128
            }
        }
    }

    /// Reads a balance, prunes expired lock slots and recomputes both the
    /// locked amount and voting power. Returns the refreshed balance where
    /// `amount` is the total holding and `amount - locked_amount` is spendable.
    fn refresh_balance(env: &Env, address: &Address) -> Balance {
        let mut balance = Self::read_balance_raw(env, address);
        let now = env.ledger().timestamp();

        let mut locks = Self::read_locks(env);
        let slots = locks.get(address.clone()).unwrap_or_else(|| Vec::new(env));

        let mut active = Vec::new(env);
        let mut total_locked: i128 = 0;
        for slot in slots.iter() {
            let locked = Self::slot_locked(&slot, now);
            if locked > 0 {
                total_locked = total_locked
                    .checked_add(locked)
                    .unwrap_or_else(|| panic_with_error!(env, RWATokenError::Overflow));
                active.push_back(slot);
            }
        }

        // Expired slots are removed on read.
        if active.len() != slots.len() {
            if active.is_empty() {
                locks.remove(address.clone());
            } else {
                locks.set(address.clone(), active);
            }
            env.storage().instance().set(&locks_key(env), &locks);
        }

        balance.locked_amount = total_locked;
        balance.voting_power = total_locked;
        env.storage().instance().set(address, &balance);
        balance
    }

    // ── token operations ──────────────────────────────────────────────────────

    pub fn mint(env: Env, auth: Address, to: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut token_info = Self::read_token_info(&env);

        if token_info.is_paused {
            panic_with_error!(&env, RWATokenError::TokenPaused);
        }

        token_info.total_supply = token_info
            .total_supply
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        let mut balance = Self::refresh_balance(&env, &to);
        balance.amount = balance
            .amount
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));
        env.storage().instance().set(&to, &balance);

        env.events().publish(
            (Symbol::new(&env, "mint"), to.clone()),
            (amount, env.ledger().timestamp()),
        );
    }

    pub fn burn(env: Env, from: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        let token_info = Self::read_token_info(&env);

        if token_info.is_paused {
            panic_with_error!(&env, RWATokenError::TokenPaused);
        }

        Self::check_version(&env);

        let mut balance = Self::refresh_balance(&env, &from);
        let spendable = balance.amount.saturating_sub(balance.locked_amount);
        if spendable < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        if !Self::check_transfer_compliance(env.clone(), from.clone(), from.clone(), amount) {
            panic_with_error!(&env, RWATokenError::ComplianceCheckFailed);
        }

        balance.amount = balance
            .amount
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));
        env.storage().instance().set(&from, &balance);

        let mut token_info = Self::read_token_info(&env);
        token_info.total_supply = token_info
            .total_supply
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        env.events().publish(
            (Symbol::new(&env, "burn"), from.clone()),
            (amount, env.ledger().timestamp()),
        );
    }

    fn transfer_internal(env: &Env, from: &Address, to: &Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(env, RWATokenError::InvalidAmount);
        }

        Self::check_version(env);

        let token_info = Self::read_token_info(env);

        if token_info.is_paused || token_info.is_frozen {
            panic_with_error!(env, RWATokenError::TransferPaused);
        }

        if !Self::check_transfer_compliance(env.clone(), from.clone(), to.clone(), amount) {
            panic_with_error!(env, RWATokenError::ComplianceCheckFailed);
        }

        let mut from_balance = Self::refresh_balance(env, from);
        let mut to_balance = Self::refresh_balance(env, to);

        // Only spendable (unlocked) tokens may be transferred.
        let spendable = from_balance
            .amount
            .saturating_sub(from_balance.locked_amount);
        if spendable < amount {
            panic_with_error!(env, RWATokenError::InsufficientBalance);
        }

        from_balance.amount = from_balance
            .amount
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(env, RWATokenError::Underflow));
        to_balance.amount = to_balance
            .amount
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(env, RWATokenError::Overflow));

        env.storage().instance().set(from, &from_balance);
        env.storage().instance().set(to, &to_balance);
    }

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        // Require explicit authorisation from the sender.
        from.require_auth();
        Self::transfer_internal(&env, &from, &to, amount);

        env.events().publish(
            (Symbol::new(&env, "transfer"), from),
            (to, amount, env.ledger().timestamp()),
        );
    }

    // ── allowances (delegated transfers) ──────────────────────────────────────

    fn read_allowances(env: &Env) -> Map<(Address, Address), i128> {
        env.storage()
            .instance()
            .get(&allowances_key(env))
            .unwrap_or_else(|| Map::new(env))
    }

    fn write_allowance(env: &Env, owner: &Address, spender: &Address, amount: i128) {
        let mut allowances = Self::read_allowances(env);
        let key = (owner.clone(), spender.clone());

        if amount == 0 {
            allowances.remove(key);
            env.events().publish(
                (
                    Symbol::new(env, "approval_revoked"),
                    owner.clone(),
                    spender.clone(),
                ),
                (),
            );
        } else {
            allowances.set(key, amount);
            env.events().publish(
                (
                    Symbol::new(env, "approval_granted"),
                    owner.clone(),
                    spender.clone(),
                ),
                amount,
            );
        }

        env.storage()
            .instance()
            .set(&allowances_key(env), &allowances);
    }

    /// Grants `spender` the right to transfer up to `amount` of `owner`'s
    /// spendable tokens. An amount of zero revokes an existing allowance.
    pub fn approve(env: Env, owner: Address, spender: Address, amount: i128) {
        owner.require_auth();

        if amount < 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        Self::check_version(&env);

        if amount > 0 {
            let balance = Self::refresh_balance(&env, &owner);
            let spendable = balance.amount.saturating_sub(balance.locked_amount);
            if amount > spendable {
                panic_with_error!(&env, RWATokenError::InsufficientBalance);
            }
        }

        Self::write_allowance(&env, &owner, &spender, amount);
    }

    /// Remaining amount `spender` may transfer on behalf of `owner`.
    pub fn allowance(env: Env, owner: Address, spender: Address) -> i128 {
        Self::read_allowances(&env)
            .get((owner, spender))
            .unwrap_or(0)
    }

    /// Alias for [`RWAToken::allowance`] kept for clients that use a getter
    /// prefix.
    pub fn get_allowance(env: Env, owner: Address, spender: Address) -> i128 {
        Self::read_allowances(&env)
            .get((owner, spender))
            .unwrap_or(0)
    }

    /// Transfers `amount` from `from` to `to` using `spender`'s allowance.
    pub fn transfer_from(env: Env, spender: Address, from: Address, to: Address, amount: i128) {
        spender.require_auth();

        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        Self::check_version(&env);

        let allowances = Self::read_allowances(&env);
        let key = (from.clone(), spender.clone());
        let approved = allowances.get(key).unwrap_or(0);
        if approved < amount {
            panic_with_error!(&env, RWATokenError::InsufficientAllowance);
        }

        Self::transfer_internal(&env, &from, &to, amount);

        let remaining = approved - amount;
        Self::write_allowance(&env, &from, &spender, remaining);

        env.events().publish(
            (Symbol::new(&env, "transfer_from"), spender),
            (from, to, amount, env.ledger().timestamp()),
        );
    }

    // ── views ─────────────────────────────────────────────────────────────────

    pub fn get_token_info(env: Env) -> TokenInfo {
        Self::read_token_info(&env)
    }

    pub fn get_balance(env: Env, address: Address) -> Balance {
        Self::refresh_balance(&env, &address)
    }

    /// Spendable balance = total balance - currently locked amount.
    pub fn get_spendable_balance(env: Env, address: Address) -> i128 {
        let balance = Self::refresh_balance(&env, &address);
        balance.amount.saturating_sub(balance.locked_amount)
    }

    /// Active (non-expired) lock slots for `owner`.
    pub fn get_lock_slots(env: Env, owner: Address) -> Vec<LockSlot> {
        Self::refresh_balance(&env, &owner);
        Self::read_locks(&env)
            .get(owner)
            .unwrap_or_else(|| Vec::new(&env))
    }

    // ── locking & vesting ─────────────────────────────────────────────────────

    /// Locks tokens until `lock_period` seconds have elapsed. Legacy entry
    /// point; equivalent to a cliff vesting schedule.
    pub fn lock_tokens(env: Env, auth: Address, owner: Address, amount: i128, lock_period: u64) {
        Self::lock_tokens_with_vesting(env, auth, owner, amount, lock_period, VestingType::Cliff);
    }

    /// Locks tokens with an explicit vesting schedule.
    ///
    /// * `Cliff` — the whole amount unlocks at `until`.
    /// * `Linear` — the amount unlocks evenly from now until `until`.
    pub fn lock_tokens_with_vesting(
        env: Env,
        auth: Address,
        owner: Address,
        amount: i128,
        lock_period: u64,
        vesting_type: VestingType,
    ) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        Self::check_version(&env);

        auth.require_auth();
        if auth != owner {
            panic_with_error!(&env, RWATokenError::Unauthorized);
        }

        let balance = Self::refresh_balance(&env, &owner);
        let spendable = balance.amount.saturating_sub(balance.locked_amount);
        if spendable < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        let now = env.ledger().timestamp();
        let until = now
            .checked_add(lock_period)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));

        let mut locks = Self::read_locks(&env);
        let mut slots = locks.get(owner.clone()).unwrap_or_else(|| Vec::new(&env));
        slots.push_back(LockSlot {
            amount,
            until,
            created_at: now,
            vesting_type: vesting_type.clone(),
        });
        locks.set(owner.clone(), slots);
        env.storage().instance().set(&locks_key(&env), &locks);

        // Recompute locked amount / voting power from the updated slots.
        Self::refresh_balance(&env, &owner);

        env.events().publish(
            (Symbol::new(&env, "tokens_locked"), owner),
            (amount, lock_period, vesting_type, env.ledger().timestamp()),
        );
    }

    /// Alias for [`RWAToken::lock_tokens_with_vesting`].
    pub fn lock_tokens_vesting(
        env: Env,
        auth: Address,
        owner: Address,
        amount: i128,
        lock_period: u64,
        vesting_type: VestingType,
    ) {
        Self::lock_tokens_with_vesting(env, auth, owner, amount, lock_period, vesting_type);
    }

    /// Manually releases `amount` from the owner's active lock slots. Tokens
    /// that have already vested are released automatically by `get_balance`,
    /// so this is only needed for an early unlock.
    pub fn unlock_tokens(env: Env, auth: Address, owner: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        Self::check_version(&env);

        auth.require_auth();
        if auth != owner {
            panic_with_error!(&env, RWATokenError::Unauthorized);
        }

        let balance = Self::refresh_balance(&env, &owner);
        if balance.locked_amount < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        let now = env.ledger().timestamp();
        let mut locks = Self::read_locks(&env);
        let slots = locks.get(owner.clone()).unwrap_or_else(|| Vec::new(&env));

        let mut remaining = amount;
        let mut kept = Vec::new(&env);
        for slot in slots.iter() {
            let mut slot = slot.clone();
            let currently_locked = Self::slot_locked(&slot, now);

            if currently_locked <= 0 {
                // Expired slot — prune it.
                continue;
            }

            if remaining > 0 {
                let take = currently_locked.min(remaining);
                if take >= currently_locked {
                    slot.amount = 0;
                } else if slot.vesting_type == VestingType::Cliff {
                    slot.amount -= take;
                } else {
                    // Convert the released amount back into the slot's
                    // original amount so the remaining linear schedule stays
                    // consistent.
                    let duration = slot.until.saturating_sub(slot.created_at) as u128;
                    let remaining_duration = slot.until.saturating_sub(now) as u128;
                    let new_locked = (currently_locked - take) as u128;
                    slot.amount = if remaining_duration == 0 || duration == 0 {
                        new_locked as i128
                    } else {
                        (new_locked.checked_mul(duration).unwrap_or(u128::MAX) / remaining_duration)
                            as i128
                    };
                    if slot.amount > currently_locked {
                        slot.amount = currently_locked - take;
                    }
                }
                remaining -= take;
            }

            if Self::slot_locked(&slot, now) > 0 {
                kept.push_back(slot);
            }
        }

        if remaining > 0 {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        if kept.is_empty() {
            locks.remove(owner.clone());
        } else {
            locks.set(owner.clone(), kept);
        }
        env.storage().instance().set(&locks_key(&env), &locks);

        Self::refresh_balance(&env, &owner);

        env.events().publish(
            (Symbol::new(&env, "tokens_unlocked"), owner),
            (amount, env.ledger().timestamp()),
        );
    }

    // ── metadata ──────────────────────────────────────────────────────────────

    /// Updates mutable metadata entries. Immutable token fields (`name`,
    /// `symbol`, `decimals`, `created_at`) cannot be changed through metadata.
    ///
    /// Emits `metadata_updated` containing only the keys whose values changed.
    pub fn update_metadata(env: Env, auth: Address, updates: Map<Symbol, String>) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        if updates.len() > MAX_METADATA_KEYS {
            panic_with_error!(&env, RWATokenError::MetadataLimitExceeded);
        }

        let name_key = Symbol::new(&env, "name");
        let symbol_key = Symbol::new(&env, "symbol");
        let decimals_key = Symbol::new(&env, "decimals");
        let created_at_key = Symbol::new(&env, "created_at");

        for (key, value) in updates.iter() {
            if key == name_key || key == symbol_key || key == decimals_key || key == created_at_key
            {
                panic_with_error!(&env, RWATokenError::MetadataKeyImmutable);
            }
            if value.len() > MAX_METADATA_VALUE_LEN {
                panic_with_error!(&env, RWATokenError::MetadataValueTooLong);
            }
        }

        let mut token_info = Self::read_token_info(&env);
        let mut changed = Vec::new(&env);

        for (key, value) in updates.iter() {
            match token_info.metadata.get(key.clone()) {
                Some(existing) if existing == value => {}
                _ => changed.push_back(key.clone()),
            }
            token_info.metadata.set(key, value);
        }

        if token_info.metadata.len() > MAX_METADATA_KEYS {
            panic_with_error!(&env, RWATokenError::MetadataLimitExceeded);
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        env.events()
            .publish((Symbol::new(&env, "metadata_updated"), auth), changed);
    }

    // ── delegated voting (liquid democracy) ───────────────────────────────────
    //
    // A holder may delegate its voting power to a representative. Delegated
    // power = own + delegated, with a max chain depth of 1 (representatives
    // cannot re-delegate the power they receive). See `shared_governance.rs`.

    /// Delegates `owner`'s voting power to `delegate` (replaces any previous
    /// delegation from `owner`). Panics on self-delegation and on any attempt
    /// to form a delegation chain longer than 1.
    pub fn delegate_votes(env: Env, owner: Address, delegate: Address) {
        let balance = Self::refresh_balance(&env, &owner);
        crate::shared_governance::delegate_votes(env, owner, delegate, balance.voting_power);
    }

    /// Removes `owner`'s active delegation, returning its voting power.
    pub fn undelegate_votes(env: Env, owner: Address) {
        crate::shared_governance::undelegate_votes(env, owner);
    }

    /// The representative `owner` currently delegates to, if any.
    pub fn get_delegation(env: Env, owner: Address) -> Option<Address> {
        crate::shared_governance::get_delegation(&env, &owner)
    }

    /// Number of holders currently delegating to `delegate`.
    pub fn get_delegated_count(env: Env, delegate: Address) -> u32 {
        crate::shared_governance::get_delegated_count(&env, &delegate)
    }

    /// Total voting power `delegate` holds on behalf of its delegators.
    pub fn get_delegated_voting_power(env: Env, delegate: Address) -> i128 {
        crate::shared_governance::get_delegated_voting_power(&env, &delegate)
    }

    /// Effective voting power of `address` = own power (unless delegated away)
    /// + power received from direct delegators.
    pub fn get_effective_voting_power(env: Env, address: Address) -> i128 {
        let balance = Self::refresh_balance(&env, &address);
        crate::shared_governance::get_effective_voting_power(&env, &address, balance.voting_power)
    }

    pub fn pause(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        let mut token_info = Self::read_token_info(&env);

        token_info.is_paused = true;
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        env.events().publish(
            (Symbol::new(&env, "token_paused"), auth),
            Symbol::new(&env, "paused"),
        );
    }

    pub fn unpause(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        let mut token_info = Self::read_token_info(&env);

        token_info.is_paused = false;
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        env.events().publish(
            (Symbol::new(&env, "token_unpaused"), auth),
            Symbol::new(&env, "unpaused"),
        );
    }

    pub fn freeze(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        let mut token_info = Self::read_token_info(&env);

        token_info.is_frozen = true;
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        env.events().publish(
            (Symbol::new(&env, "token_frozen"), auth),
            Symbol::new(&env, "frozen"),
        );
    }

    pub fn unfreeze(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        let mut token_info = Self::read_token_info(&env);

        token_info.is_frozen = false;
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        env.events().publish(
            (Symbol::new(&env, "token_unfrozen"), auth),
            Symbol::new(&env, "unfrozen"),
        );
    }

    pub fn calculate_voting_power(
        env: Env,
        balance: i128,
        model: crate::shared_governance::VotingModel,
    ) -> i128 {
        crate::shared_governance::calculate_voting_power(balance, model)
    }

    pub fn get_proposal_vote_power(
        env: Env,
        proposal_id: u64,
        address: Address,
    ) -> crate::shared_governance::VotePowerInfo {
        let balance = Self::get_balance(env.clone(), address.clone()).amount;
        crate::shared_governance::get_proposal_vote_power(&env, proposal_id, &address, balance)
    }

    fn check_transfer_compliance(env: Env, from: Address, to: Address, amount: i128) -> bool {
        let token_info = Self::read_token_info(&env);

        let registry = ComplianceRegistryClient::new(&env, &token_info.compliance_registry);
        registry.check_compliance(&from, &to, &amount)
    }

    fn check_outbound_compliance(env: Env, from: Address, amount: i128) -> bool {
        let token_info = Self::read_token_info(&env);

        let registry = ComplianceRegistryClient::new(&env, &token_info.compliance_registry);
        registry.check_outbound_participant(&from, &amount)
    }
}
