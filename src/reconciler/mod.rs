/// Reconciler module root - re-exports main entry point.
///
/// This module coordinates reconciliation between SimpleFIN and LunchFlow data sources.
pub mod db;
pub mod lib;

// Re-export main entry point for convenience
pub use lib::run;

/// Re-export unified transaction types from db module for convenience
pub use db::{
    AccountMatchRow, UnifiedTransaction, UserAccountRule, UserReconciliationAction,
    delete_user_account_rule, get_account_matches, get_lf_name_preferences,
    get_unified_transactions, get_user_account_rules, insert_user_account_rule,
    upsert_name_preference,
};
