pub mod mapping;
pub mod routes;

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FdxCurrency {
    pub currency_code: String,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FdxAccount {
    pub account_id: String,
    pub account_type: String,
    pub display_name: String,
    pub currency: FdxCurrency,
    pub current_balance: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available_balance: Option<f64>,
    pub balance_date: String,
}

#[derive(Serialize, ToSchema)]
pub struct FdxPage {
    pub total: usize,
}

#[derive(Serialize, ToSchema)]
pub struct FdxAccountList {
    pub accounts: Vec<FdxAccount>,
    pub page: FdxPage,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FdxTransaction {
    pub transaction_id: String,
    pub posted_timestamp: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transaction_timestamp: Option<String>,
    pub amount: f64,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payee: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memo: Option<String>,
    #[schema(value_type = String, example = "POSTED")]
    pub status: &'static str,
    #[schema(value_type = String, example = "DEBIT")]
    pub debit_credit_memo: &'static str,
}

#[derive(Serialize, ToSchema)]
pub struct FdxTransactionList {
    pub transactions: Vec<FdxTransaction>,
    pub page: FdxPage,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct FdxHolding {
    pub holding_id: String,
    pub currency: FdxCurrency,
    pub position_date: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub market_value: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cost_basis: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purchase_price: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub units: Option<f64>,
}

#[derive(Serialize, ToSchema)]
pub struct FdxHoldingList {
    pub holdings: Vec<FdxHolding>,
    pub page: FdxPage,
}

#[derive(Serialize, ToSchema)]
#[serde(rename_all = "camelCase")]
pub struct HealthResponse {
    #[schema(value_type = String, example = "ok")]
    pub status: &'static str,
    pub last_fetched: Option<String>,
    pub fetch_error: Option<String>,
}

/// Error body returned on 4xx/5xx responses.
#[derive(Serialize, Deserialize, ToSchema)]
pub struct ErrorResponse {
    pub error: String,
}
