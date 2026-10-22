use soroban_sdk::{contracterror, contracttype, Env, Map, Symbol, Vec, panic_with_error, String};
use crate::asset_factory::AssetConfig;
use crate::asset_class_handlers::{AssetClassError, validate_metadata_size};

#[contracterror]
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd, Ord)]
pub enum CarbonCreditError {
    InvalidVintage = 1,
    InvalidVerificationStandard = 2,
}

#[contracttype]
#[derive(Clone)]
pub struct CarbonCreditConfig {
    pub project_id: Symbol,
    pub vintage_year: u32,
    pub retirement_functionality: bool,
    pub project_metadata: Map<Symbol, Symbol>,
    pub verification_standard: Symbol,
    pub carbon_offset_amount: i128,
}

pub fn create_carbon_credit_config(
    env: Env,
    base_config: AssetConfig,
    carbon_config: CarbonCreditConfig,
) -> AssetConfig {
    validate_metadata_size(&env, base_config.metadata.len(), 100);

    let current_year = ((env.ledger().timestamp() / 31536000) + 1970) as u32;
    if carbon_config.vintage_year < 1990 || carbon_config.vintage_year > current_year {
        panic_with_error!(&env, AssetClassError::InvalidVintage);
    }

    let valid_standards = Vec::from_array(&env, [
        Symbol::new(&env, "VCS"),
        Symbol::new(&env, "GS"),
        Symbol::new(&env, "CDM"),
        Symbol::new(&env, "ACR"),
    ]);
    
    if !valid_standards.contains(&carbon_config.verification_standard) {
        panic_with_error!(&env, AssetClassError::InvalidVerificationStandard);
    }

    let mut metadata = base_config.metadata;
    metadata.set(Symbol::new(&env, "project_id"), String::from_str(&env, &carbon_config.project_id.to_string()));
    metadata.set(Symbol::new(&env, "vintage_year"), String::from_str(&env, &carbon_config.vintage_year.to_string()));
    metadata.set(Symbol::new(&env, "retirement_functionality"), String::from_str(&env, &carbon_config.retirement_functionality.to_string()));
    metadata.set(Symbol::new(&env, "verification_standard"), String::from_str(&env, &carbon_config.verification_standard.to_string()));
    metadata.set(Symbol::new(&env, "carbon_offset_amount"), String::from_str(&env, &carbon_config.carbon_offset_amount.to_string()));

    for (key, value) in carbon_config.project_metadata {
        metadata.set(key, String::from_str(&env, &value.to_string()));
    }

    AssetConfig {
        metadata,
        ..base_config
    }
}
