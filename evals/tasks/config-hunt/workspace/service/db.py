"""The database layer."""

import sqlite3

# Seconds a query may wait on a locked database.
DB_QUERY_TIMEOUT_S = 30

PAGE_SIZE = 30


def connect(path):
    return sqlite3.connect(path, timeout=DB_QUERY_TIMEOUT_S)


def page(rows, number):
    start = number * PAGE_SIZE
    return rows[start:start + PAGE_SIZE]
