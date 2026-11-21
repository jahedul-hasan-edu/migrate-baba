# Security & Safety

## 1. Credentials
* Passwords live only in the OS keychain (`keyring`): Windows Credential Manager, macOS Keychain,
  Secret Service on Linux. Profiles JSON holds no secrets. A pasted URL's password is moved to the
  keychain on save and stripped from the stored URL.
* Passwords are never logged; logs redact `password=` and URL user-info.
* Memory: password strings are dropped right after the connection config is built.

## 2. Transport
* TLS through the OS trust store (native-tls). `require` = encrypted without certificate verification
  (libpq semantics, needed for some providers with private CAs); `verify-full` = chain + hostname.
* Provider presets default to `require` (Supabase, Neon, RDS, Cloud SQL, Azure …); `verify-full` recommended
  in UI help.

## 3. Safety rails
| Rail | Behaviour |
|---|---|
| Read-only source | Source sessions run `SET default_transaction_read_only = on` — the app can never write to the source. |
| Compare-first | Deploy button only enabled after a compare with the current selection. |
| Hazard annotations | Each statement tagged; summary in deploy dialog. |
| Typed confirmation | `DATA_LOSS` hazards or Replace-mode copies require typing the target database name. |
| Production tag | Targets tagged *production* show a red banner and always require typed confirmation. |
| Test deploy | Execute + `ROLLBACK` to validate the script without changes. |
| Transactions | Deploy in a single transaction by default; data copy per-item transactions. |
| lock_timeout | Deploy sets `lock_timeout = '10s'` (configurable) to avoid blocking production. |
| Stale-compare guard | If the target changed since compare (catalog fingerprint differs), deploy asks to re-compare. |
| History | Every executed script stored locally with outcome for audit. |

## 4. Supply chain
* Dependencies pinned via `Cargo.lock`; `cargo audit` in CI; no C dependencies required on Windows/macOS.
