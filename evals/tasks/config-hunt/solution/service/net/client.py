"""The HTTP client every outgoing call goes through."""

import urllib.request

from .retry import RETRY_DELAY_S, with_retries

# How long one HTTP request may take before it is abandoned, in seconds.
REQUEST_TIMEOUT_S = 45

USER_AGENT = "service/2.3"


def fetch(url):
    """GET ``url`` and return the body, retrying transient failures."""
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})

    def attempt():
        with urllib.request.urlopen(request, timeout=REQUEST_TIMEOUT_S) as response:
            return response.read()

    return with_retries(attempt, delay=RETRY_DELAY_S)
