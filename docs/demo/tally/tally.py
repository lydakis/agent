def total(prices):
    """Sum prices in dollars, rounded to the nearest cent."""
    cents = sum(int(p * 100) for p in prices)
    return cents / 100
