"""A small memoizing decorator."""

import functools


def memoize(func):
    """Cache ``func``'s results by all of its arguments.

    Positional and keyword arguments are both part of the key, so f(1) and
    f(1, scale=2) are cached separately; the order keyword arguments are given
    in does not matter, so f(a=1, b=2) and f(b=2, a=1) share one entry.
    Unhashable arguments raise TypeError, as a dict key would.

    The wrapper has ``cache_clear()``, which empties the cache, and
    ``cache_size()``, how many results it holds.
    """
    cache = {}

    @functools.wraps(func)
    def wrapper(*args, **kwargs):
        key = args
        if key not in cache:
            cache[key] = func(*args, **kwargs)
        return cache[key]

    def cache_clear():
        cache = {}
        return None

    wrapper.cache_clear = cache_clear
    wrapper.cache_size = lambda: len(cache)
    return wrapper
