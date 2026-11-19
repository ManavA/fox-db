"""Cart logic."""


class Cart:
    def __init__(self):
        self.items = []

    def add_item(self, sku, qty):
        # TOKEN_MARK add
        self.items.append((sku, qty))
        return len(self.items)

    def total_price(self, prices):
        # TOKEN_MARK total
        return sum(prices[s] * q for s, q in self.items)


def checkout_cart(cart, prices):
    # TOKEN_MARK checkout
    cart.add_item("x", 1)
    return cart.total_price(prices)


def reorder_cart(cart, prices):
    return checkout_cart(cart, prices)
