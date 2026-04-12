# ledger-hub

## Build & run

```bash
cargo build
cargo run
```

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `SIMPLEFIN_SETUP_TOKEN` | — | One-time setup token from the SimpleFIN bridge. Only needed on first run; the claimed access URL is stored in the `config` table. Not required if `LUNCHFLOW_API_KEY` is set. |
| `LUNCHFLOW_API_KEY` | — | LunchFlow API key. If absent the LunchFlow fetcher is disabled. Not required if `SIMPLEFIN_SETUP_TOKEN` / a saved access URL is present. |
| `FETCH_INTERVAL_SECS` | `3600` | Seconds between polls. Applies to both fetchers. |
| `SERVER_ADDR` | `0.0.0.0:8080` | TCP address the HTTP server binds to. |
| `DATABASE_URL` | `sqlite://ledger-hub.db` | SQLite connection string. The file is created automatically. |

| `START_DATE_DAYS_BACK` | `90` | Days of history to fetch on the very first run. Applies to both SimpleFIN and LunchFlow. |

At least one of `SIMPLEFIN_SETUP_TOKEN` / saved access URL or `LUNCHFLOW_API_KEY` must be present or the server will start but serve no data.

## Key crates

- **simplefin 0.3** — SimpleFIN bridge client (`SimpleFINClient`, `AccountsRequest`, models including `Holding`)
- **lunchflow 0.3** — LunchFlow API client (`LunchFlowClient`, `TransactionParams`, models)
- **axum 0.8** — HTTP framework; routes use `{param}` path syntax (not `:param`)
- **sqlx 0.8 + sqlite** — async database access; migrations run automatically at startup via `sqlx::migrate!()`
- **utoipa 5** — generates OpenAPI 3.x spec from code annotations; served at `GET /openapi.json`
- **chrono** — Unix timestamp ↔ RFC 3339 conversion for FDX responses

## Module layout

```
src/
  main.rs          startup: pool, migrations, detect providers, spawn fetchers, axum server
  config.rs        Config loaded from environment variables
  error.rs         AppError implementing axum IntoResponse
  state.rs         CachedAccount + Arc<RwLock<CacheState>> (tracks both SimpleFIN and LF fetch state)
  db.rs            SQLite helpers for SimpleFIN tables: upsert_account, upsert_transaction,
                   upsert_holding, get_transactions, get_holdings, load_accounts, config KV store
  fetcher.rs       SimpleFIN background task: claim/restore access URL, poll in ≤90-day windows,
                   write DB, persist fetch state (last_fetched, next_start) for restart resilience
  fdx/
    mod.rs         FDX v6 response types with ToSchema derives; LfTransactionExt; ErrorResponse
    mapping.rs     CachedAccount/TransactionRow/HoldingRow → FDX; lf_account_to_fdx;
                   lf_transaction_to_fdx; unified_transaction_to_fdx (for REC- accounts)
    routes.rs      Axum handlers; AppState (with simplefin_configured/lunchflow_configured flags);
                   parse_account_source(); ApiDoc OpenApi struct
  lunchflow/
    mod.rs         Module root
    db.rs          SQLite helpers for LunchFlow tables: upsert_lf_account, upsert_lf_transactions,
                   replace_lf_holdings, get_all_lf_accounts, get_lf_only_accounts, get_lf_account,
                   get_lf_transactions_raw, get_unified_transactions; UnifiedTransaction type
    fetcher.rs     LunchFlow background task: poll LunchFlowClient, write lf_* tables,
                   update SharedState, trigger reconciler
    reconciler.rs  Phase 1: account matching by transaction overlap (last 20 txns, ≥60% match);
                   Phase 2: transaction matching by amount + date ±5 days;
                   writes reconciled_accounts/reconciled_transactions
migrations/
  0001_init.sql           config, accounts, transactions tables + index
  0002_v0_3_fields.sql    payee/memo columns on transactions; holdings table + index
  0003_lunchflow.sql      lf_accounts, lf_transactions, lf_holdings tables + indexes
  0004_reconciliation.sql reconciled_accounts, reconciled_transactions tables + indexes
```

## Provider modes

The server detects which providers are configured at startup:

- **SimpleFIN only** — `SIMPLEFIN_SETUP_TOKEN` set (or previously claimed URL in DB). All accounts exposed as `SIMPLEFIN-{id}`.
- **LunchFlow only** — only `LUNCHFLOW_API_KEY` set. All accounts exposed as `LUNCHFLOW-{id}`. No reconciliation.
- **Both** — both keys present. Reconciliation runs after each LunchFlow fetch. Matched accounts become `REC-{sfin_id}`; unmatched accounts keep their source prefix.

## Account ID scheme

Account IDs in all FDX endpoints are prefixed by source:

| Prefix | Source | Condition |
|---|---|---|
| `SIMPLEFIN-{sfin_id}` | SimpleFIN | No matched LunchFlow account |
| `LUNCHFLOW-{lf_id}` | LunchFlow | No matched SimpleFIN account |
| `REC-{sfin_id}` | Both (reconciled) | Reconciliation found a match |

## SimpleFIN fetch behaviour

- Fetches are split into ≤ 90-day windows (`MAX_WINDOW_SECS`) via `batch_windows()` in `fetcher.rs`.
- `last_fetched` and `next_start` are persisted to the `config` table after each successful fetch.
- On restart, the fetcher reads these values and waits out any remaining interval before the next poll.

## LunchFlow fetch behaviour

- No window batching needed; `TransactionParams.from`/`to` are sent as YYYY-MM-DD strings.
- Balances fetched per-account via `get_balance()`; accounts upserted to `lf_accounts`.
- Active accounts only: `get_transactions()` with `include_pending: true`.
- Holdings: `get_holdings()` with graceful skip on `Error::HoldingsNotSupported`; lf_holdings are delete-and-reinserted per account (no stable server id).
- `lf_last_fetched` persisted to `config` table and mirrored to `SharedState` for `/health` accuracy.

## Reconciliation

Runs in `lunchflow/reconciler.rs` after each successful LunchFlow fetch cycle.

**Phase 1 — account matching:**
- Currency mismatch (both sides known) immediately disqualifies a pair.
- Fetches the last 20 transactions from each source for every (`lf_account`, `sfin_account`) candidate pair.
- Greedy one-to-one match: counts how many sfin transactions can be paired with a lf transaction where `|amount_diff| < 0.01` and date within ±5 calendar days.
- Confidence = `matched / min(sfin_count, lf_count)`. Requires at least 3 transactions in the smaller set.
- Pairs scoring ≥ 0.6 → `status = 'matched'` in `reconciled_accounts`; unmatched accounts get `sfin_only` or `lf_only` rows with the other FK as NULL.

**Phase 2 — transaction matching (per matched account pair):**
- Candidate filter: `|sfin.amount - lf.amount| < 0.01` AND date within ±5 calendar days.
- Scores by date proximity (exact +0.4, ±1d +0.25, ±2d +0.15, ±3d +0.1, ±4-5d +0.05) and description/merchant substring match (+0.15).
- Pairs scoring ≥ 0.6 → `status = 'matched'` in `reconciled_transactions`; unmatched → `sfin_only` or `lf_only`.

Reconciliation status values exposed in API responses: "matched" | "sfin_only" | "lf_only" | "unreconciled" (before reconciliation has run).

## SimpleFIN client notes

- `SimpleFINClient::claim(token)` is async — POSTs to the bridge to exchange the setup token for an access URL.
- `SimpleFINClient::access_url_str()` returns the access URL as `&str`; used in `fetcher.rs` to persist it after the first claim.
- `SimpleFINClient::from_access_url(url)` restores a client from a previously saved access URL without a network call.
- `AccountsRequest` fields: `start_date: Option<i64>`, `end_date: Option<i64>`, `pending: bool`, `accounts: Vec<String>`, `balances_only: bool`.
- `Account` now includes `holdings: Vec<Holding>` (SimpleFIN 0.3).
- `Transaction` now includes `payee: Option<String>` and `memo: Option<String>` (SimpleFIN 0.3).

## LunchFlow client notes

- `LunchFlowClient::new(api_key)` enforces HTTPS.
- `list_accounts()` → `Vec<Account>` (id: u64, name, institution_name, provider, currency, status).
- `get_balance(account_id)` → `Balance` (amount: f64, currency).
- `get_transactions(account_id, TransactionParams)` → `Vec<Transaction>` (id: Option<String>, amount: f64, date: "YYYY-MM-DD", merchant, description, is_pending).
- `get_holdings(account_id)` → `Holdings`; returns `Error::HoldingsNotSupported` for non-investment accounts.
- Pending transactions have `id: None`; a synthetic key "lf_pending_{account_id}_{date}"` is generated for upsert.

## OpenAPI spec

- Generated at compile time by utoipa from `#[utoipa::path]` on handlers and `#[derive(ToSchema)]` on response types.
- `ApiDoc` struct in `src/fdx/routes.rs` lists all paths and schemas.
- Served as `GET /openapi.json`; no embedded Swagger UI (utoipa-swagger-ui does not yet support axum 0.8).
- To browse: `npx @redocly/cli preview-docs http://localhost:8080/openapi.json` or paste the URL into Swagger Editor / Postman.

## FDX endpoints

```
GET /health
GET /openapi.json
GET /fdx/v6/accounts
GET /fdx/v6/accounts/{accountId}
GET /fdx/v6/accounts/{accountId}/transactions?startTime=<RFC3339>&endTime=<RFC3339>
GET /fdx/v6/accounts/{accountId}/holdings
```

`{accountId}` must include the source prefix (`SIMPLEFIN-`, `LUNCHFLOW-`, or `REC-`).

For `REC-` accounts, `/transactions` returns the unified view: every transaction includes `reconciliationStatus` and `matchConfidence`; matched transactions also include a nested `lunchflowTransaction` object.

`/holdings` for `LUNCHFLOW-` accounts returns an empty list (LunchFlow holding structure does not map to FDX holdings).

No authentication — delegate to a reverse proxy (nginx, Caddy, Authelia, etc.).