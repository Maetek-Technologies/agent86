"""Number helpers."""


def clamp(value, low, high):
    """``value`` limited to [low, high]."""
    if low > high:
        raise ValueError("low must not exceed high")
    return max(low, min(high, value))


def percent(part, whole, digits=1):
    """``part`` as a percentage of ``whole``, rounded; 0.0 when ``whole`` is 0."""
    if whole == 0:
        return 0.0
    return round(100.0 * part / whole, digits)
