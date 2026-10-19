use soroban_sdk::{
    contract, contracterror, contractimpl, contracttype, panic_with_error, Address, Env, Map, Symbol, Vec, String,
};

use crate::compliance_registry::ComplianceRegistryClient;

const STORAGE_VERSION: u32 = 1;

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
    pub amount: i128,
    pub locked_amount: i128,
    pub voting_power: i128,
    pub last_dividend_claim: u64,
}

/// A holder of the token, as returned by `get_top_holders` (Issue #109).
#[contracttype]
#[derive(Clone)]
pub struct HolderInfo {
    pub address: Address,
    pub amount: i128,
}

#[contracttype]
pub struct TransferRestriction {
    pub max_daily_amount: i128,
    pub max_monthly_amount: i128,
    pub requires_accreditation: bool,
    pub geographic_restrictions: Vec<Symbol>,
}

#[contracttype]
#[derive(Clone)]
pub struct LockSlot {
    pub amount: i128,
    pub until: u64,
}

// ... (around line 61)
#[contract]
pub struct RWAToken;

#[contractimpl]
impl RWAToken {
// ...

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
        if env
            .storage()
            .instance()
            .has(&Symbol::new(&env, "initialized"))
        {
            panic_with_error!(&env, RWATokenError::AlreadyInitialized);
        }

        if total_supply <= 0 || decimals > 18 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        let token_info = TokenInfo {
            name,
            symbol,
            total_supply,
            decimals,
            asset_type,
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
            .set(&Symbol::new(&env, "locks"),
            &Map::<Address, LockSlot>::new(&env),
        );

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

        let mut current = ver;
        while current < STORAGE_VERSION {
            current += 1;
        }

        env.storage()
            .instance()
            .set(&Symbol::new(&env, "version"), &STORAGE_VERSION);
    }

    pub fn mint(env: Env, auth: Address, to: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        crate::shared_admin::require_admin(&env, &auth);

        Self::check_version(&env);

        let mut token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

        if token_info.is_paused {
            panic_with_error!(&env, RWATokenError::TokenPaused);
        }

        token_info.total_supply = token_info.total_supply
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        let mut balance = Self::get_balance(env.clone(), to.clone());
        balance.amount = balance.amount
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));
        env.storage().instance().set(&to, &balance);

        // Issue #109 — keep the holder registry in step with the balances.
        Self::adjust_holders(&env, &to, amount);

        env.events().publish(
            (Symbol::new(&env, "mint"), to.clone()),
            (amount, env.ledger().timestamp()),
        );
    }

    pub fn burn(env: Env, from: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        let token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::TokenInfoNotFound));

        if token_info.is_paused {
            panic_with_error!(&env, RWATokenError::TokenPaused);
        }

        Self::check_version(&env);

        let mut balance = Self::get_balance(env.clone(), from.clone());
        if balance.amount < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        if !Self::check_transfer_compliance(env.clone(), from.clone(), from.clone(), amount) {
            panic_with_error!(&env, RWATokenError::ComplianceCheckFailed);
        }

        balance.amount = balance.amount
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));
        env.storage().instance().set(&from, &balance);

        // Issue #109 — a holder that drops to zero is no longer a holder.
        Self::adjust_holders(&env, &from, -amount);

        let mut token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

        token_info.total_supply = token_info.total_supply
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

    pub fn transfer(env: Env, from: Address, to: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        // Require explicit authorisation from the sender
        from.require_auth();

        Self::check_version(&env);

        let token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

        if token_info.is_paused || token_info.is_frozen {
            panic_with_error!(&env, RWATokenError::TransferPaused);
        }

        if !Self::check_transfer_compliance(env.clone(), from.clone(), to.clone(), amount) {
            panic_with_error!(&env, RWATokenError::ComplianceCheckFailed);
        }

        let mut from_balance = Self::get_balance(env.clone(), from.clone());
        let mut to_balance = Self::get_balance(env.clone(), to.clone());

        // Only spendable (unlocked) tokens may be transferred
        let spendable = from_balance.amount
            .checked_sub(from_balance.locked_amount)
            .unwrap_or(0);
        if spendable < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        from_balance.amount = from_balance.amount
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));
        to_balance.amount = to_balance.amount
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));

        env.storage().instance().set(&from, &from_balance);
        env.storage().instance().set(&to, &to_balance);

        // Issue #109 — keep the holder registry in step with the balances.
        Self::adjust_holders(&env, &from, -amount);
        Self::adjust_holders(&env, &to, amount);

        env.events().publish(
            (Symbol::new(&env, "transfer"), from.clone()),
            (to, amount, env.ledger().timestamp()),
        );
    }

    /// Admin-initiated transfer that does not need the holder's consent.
    ///
    /// Issue #107 — this is the compliance-enforced seizure path: the admin
    /// authorizes the transfer on its own authority, the holder does not have
    /// to sign it, and the compliance check is not applied to the holder
    /// (the point of a seizure is that the holder does not currently comply).
    /// The token must not be paused or frozen, and the balance is still
    /// checked, so a seizure can never mint tokens.
    ///
    /// The `forced_transfer` event is distinct from `transfer` and records
    /// the admin and the reason for the audit trail.
    pub fn force_transfer(
        env: Env,
        admin: Address,
        from: Address,
        to: Address,
        amount: i128,
        reason: Symbol,
    ) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        crate::shared_admin::require_admin(&env, &admin);

        Self::check_version(&env);

        let token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

        if token_info.is_paused || token_info.is_frozen {
            panic_with_error!(&env, RWATokenError::TransferPaused);
        }

        let mut from_balance = Self::get_balance(env.clone(), from.clone());
        let mut to_balance = Self::get_balance(env.clone(), to.clone());

        if from_balance.amount < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        from_balance.amount = from_balance.amount
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));
        to_balance.amount = to_balance.amount
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));

        env.storage().instance().set(&from, &from_balance);
        env.storage().instance().set(&to, &to_balance);

        // Issue #109 — keep the holder registry in step with the balances.
        Self::adjust_holders(&env, &from, -amount);
        Self::adjust_holders(&env, &to, amount);

        env.events().publish(
            (Symbol::new(&env, "forced_transfer"), from.clone()),
            (admin, to, amount, reason, env.ledger().timestamp()),
        );
    }

    pub fn get_token_info(env: Env) -> TokenInfo {
        env.storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); })
    }

    pub fn get_balance(env: Env, address: Address) -> Balance {
        env.storage().instance().get(&address).unwrap_or(Balance {
            amount: 0,
            locked_amount: 0,
            voting_power: 0,
            last_dividend_claim: 0,
        })
    }

    /// Records a balance change in the holder registry.
    ///
    /// Issue #109 — the contract keeps one balance per address in instance
    /// storage, and Soroban offers no way to iterate instance storage, so the
    /// holders are tracked explicitly. The balance record stays authoritative;
    /// the registry mirrors it so the holder views do not have to read every
    /// balance. An entry is dropped once the balance reaches zero, so the
    /// registry only ever holds non-zero holders.
    fn adjust_holders(env: &Env, address: &Address, amount: i128) {
        let key = Symbol::new(env, "holders");
        let mut holders: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&key)
            .unwrap_or(Map::new(env));

        let updated = holders.get(address.clone()).unwrap_or(0) + amount;
        if updated > 0 {
            holders.set(address.clone(), updated);
        } else {
            holders.remove(address.clone());
        }

        env.storage().instance().set(&key, &holders);
    }

    /// The number of addresses that currently hold a non-zero balance.
    pub fn get_holder_count(env: Env) -> u32 {
        let holders: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "holders"))
            .unwrap_or(Map::new(&env));

        holders.len()
    }

    /// The largest holders, ordered by balance (descending).
    ///
    /// `limit` is capped at 100 so a single call cannot ask for an unbounded
    /// ranking. The registry is one storage entry, so the ranking is computed
    /// in memory rather than by scanning the contract's storage.
    pub fn get_top_holders(env: Env, limit: u32) -> Vec<HolderInfo> {
        const MAX_TOP_HOLDERS: u32 = 100;

        if limit == 0 {
            return Vec::new(&env);
        }
        let limit = if limit > MAX_TOP_HOLDERS {
            MAX_TOP_HOLDERS
        } else {
            limit
        };

        let holders: Map<Address, i128> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "holders"))
            .unwrap_or(Map::new(&env));

        let mut ranked: Vec<HolderInfo> = Vec::new(&env);
        for entry in holders.iter() {
            let (address, amount) = entry;
            let holder = HolderInfo { address, amount };

            // Insert in descending order of balance.
            let mut index = 0u32;
            while index < ranked.len() {
                if ranked.get(index).unwrap().amount <= amount {
                    break;
                }
                index += 1;
            }
            ranked.insert(index, holder);
        }

        while ranked.len() > limit {
            ranked.pop_back();
        }

        ranked
    }

    pub fn lock_tokens(env: Env, auth: Address, owner: Address, amount: i128, lock_period: u64) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        Self::check_version(&env);

        auth.require_auth();
        if auth != owner {
            panic_with_error!(&env, RWATokenError::Unauthorized);
        }

        let mut balance = Self::get_balance(env.clone(), owner.clone());
        if balance.amount < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        balance.amount = balance.amount
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));
        balance.locked_amount = balance.locked_amount
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));
        balance.voting_power = balance.voting_power
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));

        env.storage().instance().set(&owner, &balance);

        let mut locks: Map<Address, LockSlot> = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "locks"))
            .unwrap_or(Map::new(&env));

        let slot = LockSlot {
            amount,
            until: env.ledger().timestamp()
                .checked_add(lock_period)
                .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow)),
        };
        locks.set(owner.clone(), slot);
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "locks"), &locks);

        env.events().publish(
            (Symbol::new(&env, "tokens_locked"), owner),
            (amount, lock_period, env.ledger().timestamp()),
        );
    }

    pub fn unlock_tokens(env: Env, auth: Address, owner: Address, amount: i128) {
        if amount <= 0 {
            panic_with_error!(&env, RWATokenError::InvalidAmount);
        }

        Self::check_version(&env);

        auth.require_auth();
        if auth != owner {
            panic_with_error!(&env, RWATokenError::Unauthorized);
        }

        let mut balance = Self::get_balance(env.clone(), owner.clone());
        if balance.locked_amount < amount {
            panic_with_error!(&env, RWATokenError::InsufficientBalance);
        }

        balance.locked_amount = balance.locked_amount
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));
        balance.amount = balance.amount
            .checked_add(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Overflow));
        balance.voting_power = balance.voting_power
            .checked_sub(amount)
            .unwrap_or_else(|| panic_with_error!(&env, RWATokenError::Underflow));

        env.storage().instance().set(&owner, &balance);

        env.events().publish(
            (Symbol::new(&env, "tokens_unlocked"), owner),
            (amount, env.ledger().timestamp()),
        );
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
        let balance = Self::get_balance(env.clone(), owner.clone());
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
        let balance = Self::get_balance(env.clone(), address.clone());
        crate::shared_governance::get_effective_voting_power(&env, &address, balance.voting_power)
    }

    pub fn pause(env: Env, auth: Address) {
        crate::shared_admin::require_admin(&env, &auth);
        Self::check_version(&env);

        let mut token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

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

        let mut token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

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

        let mut token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

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

        let mut token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

        token_info.is_frozen = false;
        env.storage()
            .instance()
            .set(&Symbol::new(&env, "token_info"), &token_info);

        env.events().publish(
            (Symbol::new(&env, "token_unfrozen"), auth),
            Symbol::new(&env, "unfrozen"),
        );
    }

    pub fn calculate_voting_power(env: Env, balance: i128, model: crate::shared_governance::VotingModel) -> i128 {
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
        let token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

        let registry = ComplianceRegistryClient::new(&env, &token_info.compliance_registry);
        registry.check_compliance(&from, &to, &amount)
    }

    fn check_outbound_compliance(env: Env, from: Address, amount: i128) -> bool {
        let token_info: TokenInfo = env
            .storage()
            .instance()
            .get(&Symbol::new(&env, "token_info"))
            .unwrap_or_else(|| { panic_with_error!(&env, RWATokenError::TokenInfoNotFound); });

        let registry = ComplianceRegistryClient::new(&env, &token_info.compliance_registry);
        registry.check_outbound_participant(&from, &amount)
    }
}
