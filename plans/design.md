# SimpleFIN → FDX Server Design

## Overview

A Rust HTTP server that uses the `simplefin` crate to periodically poll a SimpleFIN bridge for
account and transaction data, persists transactions in SQLite, and serves them via an FDX-compatible
REST API. Authentication is delegated entirely to a reverse proxy.

---

## Goals

- Expose financial data from a SimpleFIN bridge via an FDX-compatible API
- Fetch fresh data on a configurable schedule without blocking reads
- Persist transactions durably in SQLite so history survives restarts and grows over time
- No built-in auth (assumes reverse proxy, e.g. nginx, Caddy, Authelia)
- Single binary, minimal configuration

---

## Crate Dependencies

| Crate | Purpose |
|---|---|
| `simplefin` | Fetch accounts and transactions from the SimpleFIN bridge |
| `axum` | HTTP framework |
| `tokio` | Async runtime + background task scheduling |
| `serde` / `serde_json` | JSON serialization |
| `tower-http` | Request tracing / logging middleware |
| `sqlx` + `sqlite` | Persistent transaction store |
| `figment` or `config` | Configuration loading (env vars + optional file) |
| `thiserror` | Error types |
| `tracing` / `tracing-subscriber` | Structured logging |
| `jiff` | Timestamp handling (already used by simplefin) |

---

## Configuration

Loaded from environment variables (with optional `.env` / config file fallback):

```
SIMPLEFIN_SETUP_TOKEN   # One-time setup token from the bridge (used on first start)
SIMPLEFIN_ACCESS_URL    # Persisted access URL after setup (stored in DB after first claim)
FETCH_INTERVAL_SECS     # How often to poll SimpleFIN (default: 3600)
SERVER_ADDR             # Bind address (default: 0.0.0.0:8080)
DATABASE_URL            # SQLite path (default: sqlite://ledger-hub.db)
START_DATE_DAYS_BACK    # How many days of history to fetch on first run (default: 90)
```

---

## Architecture

```
┌──────────────────────────────────────────────────────────┐
│                     Tokio Runtime                        │
│                                                          │
│  ┌─────────────────┐   upsert    ┌──────────────────┐   │
│  │  Fetcher Task   │────────────▶│  SQLite (sqlx)   │   │
│  │  (background)   │             │  transactions    │   │
│  └────────┬────────┘             │  accounts        │   │
│           │ write accounts       │  config          │   │
│           ▼                      └────────┬─────────┘   │
│  ┌────────────────────────┐               │ query       │
│  │  Shared Account Cache  │               │             │
│  │  Arc<RwLock<...>>      │               │             │
│  └────────────────────────┘               │             │
│                                           │             │
│  ┌────────────────────────────────────────▼───────────┐  │
│  │  Axum HTTP Server                                  │  │
│  │  GET /fdx/v6/accounts                              │  │
│  │  GET /fdx/v6/accounts/:id                          │  │
│  │  GET /fdx/v6/accounts/:id/transactions  ──▶ SQLite │  │
│  │  GET /health                                       │  │
│  └────────────────────────────────────────────────────┘  │
└──────────────────────────────────────────────────────────┘
```

### Shared State

Account data (balances, names) is small and changes with each fetch — it is held in memory as
`Arc<RwLock<CacheState>>` and updated after each successful poll.

Transactions are written to SQLite and queried directly from there, making the full history
available without holding it all in memory.

```rust
struct CacheState {
    accounts: Vec<Account>,        // from simplefin::models::Account
    last_fetched: Option<Timestamp>,
    fetch_error: Option<String>,
}
```

---

## Database Schema

Migrations are managed with `sqlx::migrate!` and run at startup.

```sql
-- config: persists the claimed access URL so a setup token is only consumed once
CREATE TABLE IF NOT EXISTS config (
    key   TEXT PRIMARY KEY,
    value TEXT NOT NULL
);

-- accounts: snapshot of latest account state (upserted on each fetch)
CREATE TABLE IF NOT EXISTS accounts (
    id                TEXT PRIMARY KEY,
    name              TEXT NOT NULL,
    currency          TEXT NOT NULL,
    balance           TEXT NOT NULL,            -- numeric string, preserve precision
    balance_date      INTEGER NOT NULL,          -- Unix timestamp (seconds)
    available_balance TEXT,                      -- nullable; SimpleFIN field is optional
    conn_id           TEXT,
    extra             TEXT                       -- JSON blob (SimpleFIN Account.extra)
);

-- transactions: append-only log, upserted by id to handle re-fetches
CREATE TABLE IF NOT EXISTS transactions (
    id             TEXT PRIMARY KEY,
    account_id     TEXT NOT NULL REFERENCES accounts(id),
    posted         INTEGER NOT NULL,             -- Unix timestamp; 0 = pending
    amount         TEXT NOT NULL,                -- numeric string, preserve precision
    description    TEXT NOT NULL,
    transacted_at  INTEGER,                      -- nullable Unix timestamp
    pending        INTEGER NOT NULL DEFAULT 0,  -- boolean (0/1)
    extra          TEXT                          -- JSON blob (SimpleFIN Transaction.extra)
);

CREATE INDEX IF NOT EXISTS idx_txn_account_posted
    ON transactions (account_id, posted);
```

Amounts are stored as strings (e.g. `"-42.50"`) to avoid floating-point rounding. The FDX
response serializes them as JSON numbers by parsing the string at read time.

---

## Background Fetcher

Runs as a `tokio::spawn`'d task at startup:

1. On startup, read `SIMPLEFIN_ACCESS_URL` from the `config` table.
   - If absent, use `SIMPLEFIN_SETUP_TOKEN` to claim an access URL via `SimpleFINClient`, then
     store it in the `config` table.
2. Determine the fetch window start:
   - First run: `now - START_DATE_DAYS_BACK days`
   - Subsequent runs: `last_fetched - 24h` (overlap to catch late-arriving transactions)
3. Enter a loop:
   - Call `SimpleFINClient::get_accounts(AccountsRequest { start_date, end_date, pending: false, .. })` with the fetch window.
   - On success:
     - Upsert all accounts into the `accounts` table.
     - Upsert all transactions into the `transactions` table (by `id`).
     - Update `last_fetched` in `CacheState`.
     - Update account list in `CacheState`.
   - On error: log it, write to `CacheState.fetch_error`, do not modify DB or clear accounts.
   - Sleep for `FETCH_INTERVAL_SECS`.

---

## FDX API Endpoints

FDX v6 is the target version. All responses are `application/json`.

### `GET /fdx/v6/accounts`

Returns accounts from the in-memory cache (latest balances from last fetch).

**Response:**
```json
{
  "accounts": [
    {
      "accountId": "...",
      "accountType": "OTHER",
      "displayName": "My Checking",
      "currency": { "currencyCode": "USD" },
      "currentBalance": 1234.56,
      "balanceDate": "2024-01-15T00:00:00Z"
    }
  ],
  "page": { "total": 2 }
}
```

### `GET /fdx/v6/accounts/:accountId`

Returns a single account from the in-memory cache. Returns 404 if not found.

### `GET /fdx/v6/accounts/:accountId/transactions`

Queries SQLite directly. Supports optional query params:
- `startTime` (ISO 8601) — filters `posted >= startTime`
- `endTime` (ISO 8601) — filters `posted <= endTime`

```sql
SELECT * FROM transactions
WHERE account_id = ?
  AND posted >= ?   -- startTime or 0
  AND posted <= ?   -- endTime or now
ORDER BY posted DESC;
```

**Response:**
```json
{
  "transactions": [
    {
      "transactionId": "...",
      "postedTimestamp": "2024-01-10T12:00:00Z",
      "amount": -42.00,
      "description": "Coffee shop",
      "status": "POSTED",
      "debitCreditMemo": "DEBIT"
    }
  ],
  "page": { "total": 1 }
}
```

### `GET /health`

```json
{
  "status": "ok",
  "lastFetched": "2024-01-15T10:00:00Z",
  "fetchError": null
}
```

---

## SimpleFIN Data Model (confirmed from source)

### AccountSet
| Field | Type | Notes |
|---|---|---|
| `errors` | `Vec<SfinError>` | defaults to empty |
| `connections` | `Vec<Connection>` | defaults to empty |
| `accounts` | `Vec<Account>` | |

### Account
| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | `String` | yes | |
| `name` | `String` | yes | |
| `currency` | `String` | yes | e.g. `"USD"` |
| `balance` | `String` | yes | numeric string; positive = credit |
| `balance_date` | `i64` | yes | UNIX timestamp |
| `conn_id` | `Option<String>` | no | |
| `available_balance` | `Option<String>` | no | serialized as `"available-balance"` |
| `transactions` | `Option<Vec<Transaction>>` | no | absent when `balances_only = true` |
| `extra` | `Option<Value>` | no | arbitrary JSON |

### Transaction
| Field | Type | Required | Notes |
|---|---|---|---|
| `id` | `String` | yes | |
| `posted` | `i64` | yes | UNIX timestamp; `0` means pending |
| `amount` | `String` | yes | numeric string |
| `description` | `String` | yes | |
| `transacted_at` | `Option<i64>` | no | UNIX timestamp |
| `pending` | `Option<bool>` | no | |
| `extra` | `Option<Value>` | no | arbitrary JSON |

### AccountsRequest (query parameters)
| Field | Type | Default | Notes |
|---|---|---|---|
| `start_date` | `Option<i64>` | — | UNIX timestamp, inclusive |
| `end_date` | `Option<i64>` | — | UNIX timestamp, exclusive |
| `pending` | `bool` | false | include pending transactions |
| `accounts` | `Vec<String>` | [] | restrict to specific IDs; empty = all |
| `balances_only` | `bool` | false | skip transactions, return balances only |

### Connection
| Field | Type | Required |
|---|---|---|
| `conn_id` | `String` | yes |
| `name` | `String` | yes |
| `org_id` | `Option<String>` | no |
| `org_url` | `Option<String>` | no |
| `sfin_url` | `Option<String>` | no |

---

## Data Mapping: SimpleFIN → FDX

| SimpleFIN field | FDX field | Notes |
|---|---|---|
| `Account.id` | `accountId` | |
| `Account.name` | `displayName` | |
| `Account.balance` (String) | `currentBalance` | parse to f64 for JSON |
| `Account.available_balance` (String) | `availableBalance` | omit if absent |
| `Account.currency` | `currency.currencyCode` | |
| `Account.balance_date` | `balanceDate` | UNIX ts → ISO 8601 |
| `Transaction.id` | `transactionId` | |
| `Transaction.posted` | `postedTimestamp` | UNIX ts → ISO 8601 |
| `Transaction.transacted_at` | `transactionTimestamp` | omit if absent |
| `Transaction.amount` (String) | `amount` + `debitCreditMemo` | parse sign to derive memo |
| `Transaction.description` | `description` | |
| `Transaction.pending` | `status` | `true` → `"PENDING"`, else `"POSTED"` |

FDX `accountType` defaults to `"OTHER"` — SimpleFIN does not expose account type.

`debitCreditMemo` is derived: `amount < 0` → `"DEBIT"`, `amount >= 0` → `"CREDIT"`.

A transaction with `posted == 0` is treated as pending regardless of the `pending` field.

---

## Module Structure

```
src/
  main.rs          # startup: config, DB migrations, spawn fetcher, start axum
  config.rs        # Config struct, env loading
  state.rs         # CacheState + Arc<RwLock<CacheState>>
  db.rs            # sqlx pool setup, query helpers for accounts + transactions
  fetcher.rs       # background fetch loop
  fdx/
    mod.rs         # FDX response types (serde structs)
    routes.rs      # axum handlers
    mapping.rs     # SimpleFIN → FDX conversion functions
  error.rs         # AppError implementing axum IntoResponse
migrations/
  0001_init.sql    # accounts, transactions, config tables + indexes
```

---

## Startup Sequence

1. Load config from env.
2. Open SQLite pool, run `sqlx::migrate!`.
3. Read access URL from `config` table; if absent, claim it using setup token and store it.
4. Load latest account snapshot from `accounts` table into `CacheState` (so the server can answer
   account requests immediately, even before the first fresh fetch completes).
5. Spawn fetcher task (runs an immediate fetch, then loops).
6. Start Axum server.

---

## Error Handling

- If no accounts have been fetched yet (DB empty, first run still in progress), return
  `503 Service Unavailable` from account and transaction endpoints.
- If a requested `accountId` is not in the cache, return `404 Not Found`.
- Fetch errors are logged and exposed on `/health` but do not clear the DB or in-memory cache —
  stale data is served until the next successful fetch.

---

## Non-Goals (v1)

- Authentication or authorization
- Pagination beyond total count in `page` object
- FDX consent management endpoints
- Webhooks / push notifications
- Multiple SimpleFIN bridges
- Write operations (SimpleFIN is read-only)
