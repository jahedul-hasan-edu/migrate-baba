# UI / UX Specification

## 1. Layout (standard desktop tool layout)

```
┌──────────────────────────────────────────────────────────────────────────────────────┐
│ ◆ Migrate Baba   Source [● staging ▾]  ⇄  Target [● production ▾]   Schemas [public ▾] │  ← workspace bar
├────────────┬─────────────────────────────────────────────────────────────────────────┤
│ Connections│                                                                         │
│ Schema     │                     active screen                                        │
│ Data diff  │                                                                         │
│ Migrate    │                                                                         │
│ History    │                                                                         │
│ Settings   │                                                                         │
├────────────┴─────────────────────────────────────────────────────────────────────────┤
│ status: ● connected · source 42 ms · target 180 ms · last compare 0.8 s     [Log ▴]   │  ← status bar
└──────────────────────────────────────────────────────────────────────────────────────┘
```

* Left navigation rail (icons + labels), collapsible.
* Status bar: connection health + latency, background job indicator, toggle for the log drawer.
* Environment colour (profile tag) shown as a dot + coloured border on the selector; red = production.

## 2. Screens

### 2.1 Connections
List (left) + form (right). Form: Provider preset dropdown, *Paste URL* field, Name, Host, Port,
Database, User, Password (eye toggle), SSL mode, Colour tag, Excluded schemas. Buttons: **Test**,
**Save**, **Use as source**, **Use as target**, Delete. Test result card: version, latency, user, privileges.

### 2.2 Schema Compare
* Toolbar: **Compare** (F5), options popover, kind filter chips, search box (Ctrl+F), "show identical".
* Left: results table, grouped headers *To create (n) / To alter (n) / To drop (n) / Identical (n)*;
  columns: ☑, kind icon, object name, change summary. Virtualised rows.
* Right (resizable split): tabs **Diff** (side-by-side DDL, changed lines tinted: green add, red remove,
  amber change; line numbers) and **Script** (SQL for the *selected* objects, syntax highlighted, hazards
  rendered as coloured badges in the gutter).
* Footer: selected count, hazards summary (e.g. "2 data-loss, 1 rewrite"), buttons **Copy SQL**,
  **Save…**, **Save as migration…**, **Test deploy**, **Deploy** (primary).
* Deploy dialog: summary table by phase, hazard list, options (single transaction, lock timeout), confirm
  field (type target DB name when hazards include `DATA_LOSS`), progress per statement, result.

### 2.3 Data Compare
* Toolbar: **Compare data**, chunk size, "exact counts".
* Table grid: ☑, table, source rows, target rows, status (identical ✔ / different ≠ / only source / only
  target / no PK), only-in-source, only-in-target, different, time.
* Detail panel for selected table: sample keys per category.
* Footer: **Sync selected** (with confirmation), progress.

### 2.4 Migrate Data
* Table picker grouped by schema with est. rows & size, select all/none/filter.
* Options card: mode (Replace / Append / Sync), workers slider (1–32), consistent snapshot, disable
  triggers, reset sequences, analyze after, create missing tables, stop on first error.
* Run view: overall progress bar (bytes), throughput sparkline (MB/s), elapsed / ETA; per-table rows
  with state chips (queued, copying, done, failed), rows, size, MB/s. **Cancel** button.

### 2.5 History
List of runs (kind, source → target, when, duration, outcome). Detail: script, log, stats. Copy / save.

### 2.6 Settings
Theme, UI scale, defaults (workers, chunk size, statement/lock timeout, excluded schemas, compare
options).

## 3. Interaction rules
* Never block: every network action shows an inline spinner on the triggering button; UI stays interactive.
* Long lists are virtualised; selection works with click, Shift-click ranges, Ctrl-A.
* Keyboard: `F5` compare, `Ctrl+F` search, `Ctrl+Enter` deploy dialog, `Ctrl+S` save script,
  `Ctrl+Shift+C` copy script, `Ctrl+1..6` switch screen, `Esc` close dialog.
* Errors: toast + log entry; full error (SQLSTATE, detail, hint, statement) in an expandable card.
* Destructive actions use red primary buttons and require explicit confirmation.

## 4. Visual language
* Neutral dark theme by default (follows system light/dark), one accent colour (blue), semantic colours:
  green = create/add, amber = alter, red = drop/destructive, grey = identical.
* 8-px spacing grid, 13-px UI font, monospaced 12.5-px for SQL.
* Icons are Unicode/emoji-free glyphs from the bundled font to keep the binary small.
