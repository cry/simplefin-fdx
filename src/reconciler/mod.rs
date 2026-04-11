/// Reconciler module root - re-exports main entry point.
///
/// This module coordinates reconciliation between SimpleFIN and LunchFlow data sources.
pub mod lib;

// Re-export main entry point for convenience
pub use lib::run;
