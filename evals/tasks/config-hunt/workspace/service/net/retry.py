"""Retries for transient failures."""

import time

# Seconds to wait between attempts.
RETRY_DELAY_S = 30

MAX_ATTEMPTS = 3


def with_retries(attempt, delay=RETRY_DELAY_S, attempts=MAX_ATTEMPTS):
    for number in range(attempts):
        try:
            return attempt()
        except OSError:
            if number == attempts - 1:
                raise
            time.sleep(delay)
