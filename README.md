# simplefin-server

A Rust server that polls a [SimpleFIN](https://www.simplefin.org/) bridge for account and transaction data, stores it in SQLite, and serves it via a [FDX v6](https://financialdataexchange.org/) compatible REST API.

Designed to run behind a reverse proxy (nginx, Caddy, Authelia, etc.) — no built-in authentication.

## Getting started

### 1. Get a SimpleFIN setup token

Log in to your SimpleFIN bridge and generate a setup token. It is a base64-encoded URL that looks like `aHR0cHM6...`.

### 2. Configure

Set environment variables (or write a `.env` file and export them):

```bash
export SIMPLEFIN_SETUP_TOKEN=<your token>   # only needed on first run
export DATABASE_URL=sqlite://simplefin.db   # default
export SERVER_ADDR=127.0.0.1:8080           # default: 0.0.0.0:8080
export FETCH_INTERVAL_SECS=3600             # default: hourly
export START_DATE_DAYS_BACK=90              # default: 90 days of history on first run
```

### 3. Run

```bash
cargo run --release
```

On first run the server claims a persistent access URL from the setup token and stores it in the database. Subsequent runs use the stored URL — `SIMPLEFIN_SETUP_TOKEN` is no longer needed.

Fetch state (last poll time and next window start) is also persisted to the database, so restarting the server will not trigger an immediate re-fetch if one has occurred recently.

## Docker

### Build

```bash
docker build -t simplefin-server .
```

### Run

The server stores its SQLite database at the path specified by `DATABASE_URL`. Mount a host directory so the database persists across container restarts:

```bash
mkdir -p /path/to/data

docker run -d \
  --name simplefin-server \
  -p 8080:8080 \
  -v /path/to/data:/data \
  -e DATABASE_URL=sqlite:///data/simplefin.db \
  -e SIMPLEFIN_SETUP_TOKEN=<your token> \
  simplefin-server
```

After the first run the access URL is stored in the database. You can drop `SIMPLEFIN_SETUP_TOKEN` from subsequent runs:

```bash
docker run -d \
  --name simplefin-server \
  -p 8080:8080 \
  -v /path/to/data:/data \
  -e DATABASE_URL=sqlite:///data/simplefin.db \
  simplefin-server
```

> **Note:** Use an absolute path for `DATABASE_URL` inside the container (`sqlite:///data/...` with three slashes). A relative path like `sqlite://simplefin.db` would write the database inside the container and lose it on restart.

### Docker Compose

```yaml
services:
  simplefin-server:
    build: .
    ports:
      - "8080:8080"
    volumes:
      - ./data:/data
    environment:
      DATABASE_URL: sqlite:///data/simplefin.db
      SIMPLEFIN_SETUP_TOKEN: <your token>  # remove after first run
      FETCH_INTERVAL_SECS: 3600
    restart: unless-stopped
```

## Web UI

A browser-based UI is served at `GET /`. It shows all accounts in a sidebar and lets you browse transactions (with a date range filter) and holdings for each account.

## API

All responses are `application/json`. The full OpenAPI spec is available at `GET /openapi.json` and can be loaded into any compatible viewer (Swagger UI, Redoc, Stoplight, Postman).

### `GET /`

Returns the web UI (HTML). Open in a browser to explore accounts and transactions.

### `GET /health`

```json
{
  "status": "ok",
  "lastFetched": "2024-01-15T10:00:00+00:00",
  "fetchError": null
}
```

`status` is `"degraded"` if the last fetch failed; stale data is still served.

### `GET /fdx/v6/accounts`

```json
{
  "accounts": [
    {
      "accountId": "abc123",
      "accountType": "OTHER",
      "displayName": "My Checking",
      "currency": { "currencyCode": "USD" },
      "currentBalance": 1234.56,
      "availableBalance": 1200.00,
      "balanceDate": "2024-01-15T00:00:00+00:00"
    }
  ],
  "page": { "total": 1 }
}
```

### `GET /fdx/v6/accounts/{accountId}`

Returns a single account. `404` if not found, `503` if no data has been fetched yet.

### `GET /fdx/v6/accounts/{accountId}/transactions`

Optional query parameters:

| Parameter | Format | Description |
|---|---|---|
| `startTime` | RFC 3339 | Include transactions posted on or after this time |
| `endTime` | RFC 3339 | Include transactions posted on or before this time |

```json
{
  "transactions": [
    {
      "transactionId": "txn_001",
      "postedTimestamp": "2024-01-10T12:00:00+00:00",
      "amount": -42.00,
      "description": "Coffee shop",
      "payee": "Blue Bottle Coffee",
      "memo": "Card purchase",
      "status": "POSTED",
      "debitCreditMemo": "DEBIT"
    }
  ],
  "page": { "total": 1 }
}
```

### `GET /fdx/v6/accounts/{accountId}/holdings`

Returns investment holdings for an account. Fields are omitted when not provided by the bridge.

```json
{
  "holdings": [
    {
      "holdingId": "h_001",
      "currency": { "currencyCode": "USD" },
      "positionDate": "2024-01-15T00:00:00+00:00",
      "symbol": "AAPL",
      "description": "Apple Inc.",
      "marketValue": 15234.56,
      "costBasis": 12000.00,
      "purchasePrice": 120.00,
      "units": 100.0
    }
  ],
  "page": { "total": 1 }
}
```

### `GET /openapi.json`

Returns the OpenAPI 3.x spec for this server. Load it into any OpenAPI viewer:

```bash
# Serve locally with Redoc (requires Node/npx)
npx @redocly/cli preview-docs http://localhost:8080/openapi.json

# Or open in Swagger Editor
open https://editor.swagger.io/?url=http://localhost:8080/openapi.json
```

## Fetch behaviour

- **First run**: fetches `START_DATE_DAYS_BACK` days of history (default 90), split into ≤ 90-day batches.
- **Subsequent fetches**: fetches from 24 hours before the last successful poll to catch late-arriving transactions.
- **Restart**: if the server restarts within the current fetch interval, it waits out the remaining time before polling again. If the interval has already elapsed, it polls immediately.
- **Failed fetch**: retries the same window on the next cycle. Stale data continues to be served.

## Data notes

- `accountType` is always `"OTHER"` — SimpleFIN does not expose account type.
- `status` is `"PENDING"` when `Transaction.pending` is true or `posted` timestamp is zero; otherwise `"POSTED"`.
- `payee` and `memo` are omitted from transaction responses when not provided by the bridge.
- Amounts are stored as strings in SQLite to preserve decimal precision and serialized as JSON numbers in responses.
- Transactions and holdings are upserted by ID, so overlapping fetch windows never create duplicates.

## Reverse proxy example (Caddy)

```
simplefin.example.com {
    basicauth {
        user <bcrypt-hash>
    }
    reverse_proxy 127.0.0.1:8080
}
```
