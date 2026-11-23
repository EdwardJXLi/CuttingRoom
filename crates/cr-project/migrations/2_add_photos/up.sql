CREATE TABLE photos (
    id TEXT PRIMARY KEY NOT NULL,
    path TEXT NOT NULL UNIQUE,
    present INTEGER NOT NULL DEFAULT 1 CHECK (present IN (0, 1))
) STRICT;

PRAGMA user_version = 2;
