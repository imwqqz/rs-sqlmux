# ru-sqlmux

A terminal database client in Rust for browsing tables, editing cells, and running SQL
against Postgres, MySQL and SQLite. A minimal rebuild of
[sqlmux](https://github.com/kackerx/sqlmux) that keeps the useful core
(transactional in-place editing) and drops the rest.

```
ru-sqlmux ./app.db
ru-sqlmux "postgres://me@localhost:5432/app"
ru-sqlmux "mysql://me:env:DB_PW@localhost/app"
```

The DSN is checked before anything is opened. Server URLs must carry a host,
and a bare path must point at a real SQLite file that already exists. A typo
reads `cannot connect: no such database file: .\app.db`, and an empty or
foreign file reads `... is not a SQLite database`, instead of either one being
opened as a blank database. Write `sqlite://new.db` when you want a new file
created; it is initialised immediately, so it also opens by path afterwards.

## Keys

| Where | Key | Action |
|---|---|---|
| anywhere | `Tab` | cycle focus: tables → grid → SQL → results |
| anywhere | `C-s` / `C-c` | save pending changes / quit (`?` shows all keys) |
| tables | `j k / Enter` | move, filter (`/`), open table |
| grid | `h j k l` `g G` `[ ]` | move, page; `s` sort column |
| grid | `e o d` | edit cell, insert row, delete row |
| SQL | `Enter` `C-f` `C-p/n` | run, format, history |