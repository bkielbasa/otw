#!/bin/sh
# Regenerates tests/fixtures/index.db, a small copy of the winget "source2"
# index.db. It needs the real index.db (from Public/index.db inside
# https://cdn.winget.microsoft.com/cache/source2.msix) and the sqlite3 CLI.
#
# The copy uses 512-byte pages and adds synthetic packages, so that the
# tables span several levels of B-tree pages and one row needs overflow
# pages: the reader in src/sqlite.rs has to handle all of that.
set -eu
real=$1
out=$(dirname "$0")/index.db
rm -f "$out"
sqlite3 "$out" <<SQL
PRAGMA page_size = 512;
ATTACH '$real' AS real;
CREATE TABLE [metadata]([name] TEXT PRIMARY KEY NOT NULL, [value] TEXT NOT NULL) WITHOUT ROWID;
CREATE TABLE [packages](rowid INTEGER PRIMARY KEY, [id] TEXT NOT NULL, [name] TEXT NOT NULL, [moniker] TEXT, [latest_version] TEXT NOT NULL, [arp_min_version] TEXT, [arp_max_version] TEXT, [hash] BLOB);
CREATE TABLE [tags2](rowid INTEGER PRIMARY KEY, [tag] TEXT NOT NULL);
CREATE TABLE [tags2_map]([tag] INT64 NOT NULL, [package] INT64 NOT NULL, PRIMARY KEY([tag], [package])) WITHOUT ROWID;
CREATE TABLE [commands2](rowid INTEGER PRIMARY KEY, [command] TEXT NOT NULL);
CREATE TABLE [commands2_map]([command] INT64 NOT NULL, [package] INT64 NOT NULL, PRIMARY KEY([command], [package])) WITHOUT ROWID;
INSERT INTO metadata SELECT * FROM real.metadata;
CREATE TEMP TABLE keep AS SELECT rowid AS r FROM real.packages WHERE id IN (
  '7zip.7zip', '7zip.7zr', 'Notepad++.Notepad++', 'Microsoft.WindowsTerminal',
  'Rustlang.Rustup', 'Mozilla.Firefox', 'Mozilla.Firefox.DeveloperEdition',
  'dail8859.NotepadNext', 'flos-freeware.Notepad2', 'Microsoft.XMLNotepad');
INSERT INTO packages SELECT * FROM real.packages WHERE rowid IN (SELECT r FROM keep);
INSERT INTO tags2_map SELECT * FROM real.tags2_map WHERE package IN (SELECT r FROM keep);
INSERT INTO tags2 SELECT * FROM real.tags2 WHERE rowid IN (SELECT tag FROM tags2_map);
INSERT INTO commands2_map SELECT * FROM real.commands2_map WHERE package IN (SELECT r FROM keep);
INSERT INTO commands2 SELECT * FROM real.commands2 WHERE rowid IN (SELECT command FROM commands2_map);
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600)
INSERT INTO packages (rowid, id, name, moniker, latest_version, hash)
  SELECT 100000 + i, printf('Synthetic.Package%04d', i), printf('Synthetic Package %d', i),
         NULL, printf('1.%d', i), randomblob(32) FROM n;
INSERT INTO packages (rowid, id, name, moniker, latest_version, hash)
  VALUES (200000, 'Synthetic.LongName', 'Long ' || replace(printf('%.3000c', 'x'), 'x', 'name '),
          'longname', '2.0', zeroblob(32));
INSERT INTO tags2 (rowid, tag) VALUES (900000, 'synthetic-tag');
WITH RECURSIVE n(i) AS (SELECT 1 UNION ALL SELECT i + 1 FROM n WHERE i < 600)
INSERT INTO tags2_map SELECT 900000, 100000 + i FROM n;
SQL
echo "wrote $out"
