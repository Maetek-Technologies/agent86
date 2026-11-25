"""A small memoizing decorator."""

import functools


def memoize(func):
    """Cache ``func``'s results by all of its arguments (see the task)."""
    cache = {}

    @functools.wraps(func)
    def wrapper(*args, **kwargs):
        key = (args, frozenset(kwargs.items()))
        if key not in cache:
            cache[key] = func(*args, **kwargs)
        return cache[key]

    def cache_clear():
        cache.clear()

    wrapper.cache_clear = cache_clear
    wrapper.cache_size = lambda: len(cache)
    return wrapper
