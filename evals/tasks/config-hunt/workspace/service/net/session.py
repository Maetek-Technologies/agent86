"""A pooled HTTP session for the webhook sender."""

import http.client

DEFAULT_TIMEOUT = 30  # seconds per HTTP request
POOL_SIZE = 8
CACHE_TTL_S = 30  # how long a resolved host is reused


class Session:
    def __init__(self, host, timeout=DEFAULT_TIMEOUT):
        self.host = host
        self.timeout = timeout

    def post(self, path, body):
        connection = http.client.HTTPSConnection(self.host, timeout=self.timeout)
        connection.request("POST", path, body)
        return connection.getresponse().status
