# simplefin-server

## Build & run

```bash
cargo build
cargo run
```

## Environment variables

| Variable | Default | Description |
|---|---|---|
| `SIMPLEFIN_SETUP_TOKEN` | — | One-time setup token from the SimpleFIN bridge. Only needed on first run; the claimed access URL is stored in the `config` table. |
| `FETCH_INTERVAL_SECS` | `3600` | Seconds between polls of the SimpleFIN bridge. |
| `SERVER_ADDR` | `0.0.0.0:8080` | TCP address the HTTP server binds to. |
| `DATABASE_URL` | `sqlite://simplefin.db` | SQLite connection string. The file is created automatically. |
| `START_DATE_DAYS_BACK` | `90` | Days of history to fetch on the very first run. |

## Key crates

- **simplefin 0.3** — SimpleFIN bridge client (`SimpleFINClient`, `AccountsRequest`, models including `Holding`)
- **axum 0.8** — HTTP framework; routes use `{param}` path syntax (not `:param`)
- **sqlx 0.8 + sqlite** — async database access; migrations run automatically at startup via `sqlx::migrate!()`
- **utoipa 5** — generates OpenAPI 3.x spec from code annotations; served at `GET /openapi.json`
- **chrono** — Unix timestamp ↔ RFC 3339 conversion for FDX responses

## Module layout

```
src/
  main.rs          startup: pool, migrations, fetcher task, axum server, /openapi.json route
  config.rs        Config loaded from environment variables
  error.rs         AppError implementing axum IntoResponse
  state.rs         CachedAccount + Arc<RwLock<CacheState>>
  db.rs            SQLite helpers: upsert_account, upsert_transaction, upsert_holding,
                   get_transactions, get_holdings, load_accounts, config key-value store
  fetcher.rs       Background task: claim/restore access URL, poll SimpleFIN, write DB,
                   persist fetch state (last_fetched, next_start) for restart resilience
  fdx/
    mod.rs         FDX v6 response types with ToSchema derives; ErrorResponse
    mapping.rs     CachedAccount / TransactionRow / HoldingRow → FDX types
    routes.rs      Axum handlers with #[utoipa::path] annotations; ApiDoc OpenApi struct
migrations/
  0001_init.sql    config, accounts, transactions tables + index
  0002_v0_3_fields.sql  payee/memo columns on transactions; holdings table + index
```

## SimpleFIN client notes

- `SimpleFINClient::claim(token)` is async — POSTs to the bridge to exchange the setup token for an access URL.
- `SimpleFINClient::access_url_str()` returns the access URL as `&str`; used in `fetcher.rs` to persist it after the first claim.
- `SimpleFINClient::from_access_url(url)` restores a client from a previously saved access URL without a network call.
- `AccountsRequest` fields: `start_date: Option<i64>`, `end_date: Option<i64>`, `pending: bool`, `accounts: Vec<String>`, `balances_only: bool`.
- `Account` now includes `holdings: Vec<Holding>` (SimpleFIN 0.3).
- `Transaction` now includes `payee: Option<String>` and `memo: Option<String>` (SimpleFIN 0.3).

## Fetch behaviour

- Fetches are split into ≤ 90-day windows (`MAX_WINDOW_SECS`) via `batch_windows()` in `fetcher.rs`.
- `last_fetched` and `next_start` are persisted to the `config` table after each successful fetch.
- On restart, the fetcher reads these values and waits out any remaining interval before the next poll.

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

No authentication — delegate to a reverse proxy (nginx, Caddy, Authelia, etc.).
