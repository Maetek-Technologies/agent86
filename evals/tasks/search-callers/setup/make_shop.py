"""Write ./shop, the same every time: a payments gateway and twelve modules of
six functions each, every function calling up to two functions from the
two modules below it (so there are no import cycles). Calls to the gateway are made
directly, through an alias, and through the module; there is a look-alike
function, charge_cardholder_fee, and charge_card is named in comments and
strings where nothing calls it."""

import pathlib
import random

MODULES = ["catalog", "cart", "pricing", "promotions", "orders", "invoices", "shipping",
           "returns", "subscriptions", "notifications", "admin", "reports"]
VERBS = ["build", "load", "apply", "check", "sync", "close", "open", "send", "score", "merge"]
NOUNS = ["summary", "batch", "entry", "totals", "record", "window", "queue", "draft", "ledger"]

GATEWAY = '''"""The card gateway."""


def charge_card(card, amount):
    """Charge ``amount`` to ``card``."""
    if amount <= 0:
        raise ValueError("amount must be positive")
    return {"card": card, "charged": amount}


def refund_card(card, amount):
    return {"card": card, "refunded": amount}


def charge_cardholder_fee(card):
    """A flat fee; not a charge_card call."""
    return {"card": card, "fee": 1}


def verify_card(card):
    return bool(card)
'''

GATEWAY_FUNCTIONS = ["charge_card", "refund_card", "charge_cardholder_fee", "verify_card"]


def main():
    rng = random.Random(86)
    root = pathlib.Path("shop")
    (root / "payments").mkdir(parents=True, exist_ok=True)
    (root / "__init__.py").write_text('"""The shop."""\n', encoding="utf-8")
    (root / "payments" / "__init__.py").write_text("", encoding="utf-8")
    (root / "payments" / "gateway.py").write_text(GATEWAY, encoding="utf-8")

    gateway_pool = [("payments.gateway", name) for name in GATEWAY_FUNCTIONS if name != "charge_card"]
    available = []
    used = set(GATEWAY_FUNCTIONS)
    for layer, module in enumerate(MODULES):
        names = []
        while len(names) < 6:
            name = f"{rng.choice(VERBS)}_{module.rstrip('s')}_{rng.choice(NOUNS)}"
            if name not in used:
                used.add(name)
                names.append(name)
        # How this module reaches the gateway: by name, by alias, or by module.
        style = ["name", "alias", "module"][layer % 3]
        imports = set()
        bodies = []
        local = []
        for name in names:
            # Calls reach the gateway (never charge_card at random) and the two
            # modules below, so callers stay a minority.
            pool = [entry for entry in available[-12:] + gateway_pool] + [(module, other) for other in local]
            calls = rng.sample(pool, k=min(len(pool), rng.choice([0, 1, 1, 1, 2])))
            if layer in (1, 5, 9) and name == names[0] and ("payments.gateway", "charge_card") not in calls:
                calls.append(("payments.gateway", "charge_card"))
            lines = [f"def {name}(order):", f'    """{name.replace("_", " ").capitalize()}."""']
            if rng.random() < 0.25:
                lines.append("    # Not charge_card: that happens at checkout.")
            if rng.random() < 0.2:
                lines.append('    log = "charge_card is retried by the gateway"')
            lines.append("    result = []")
            for target_module, target in calls:
                if target_module == module:
                    lines.append(f"    result.append({target}(order))")
                elif target_module == "payments.gateway" and target == "charge_card":
                    if style == "alias":
                        imports.add("from shop.payments.gateway import charge_card as take_payment")
                        lines.append("    result.append(take_payment(order, 1))")
                    elif style == "module":
                        imports.add("from shop.payments import gateway")
                        lines.append("    result.append(gateway.charge_card(order, 1))")
                    else:
                        imports.add("from shop.payments.gateway import charge_card")
                        lines.append("    result.append(charge_card(order, 1))")
                elif target_module == "payments.gateway":
                    imports.add(f"from shop.payments.gateway import {target}")
                    lines.append(f"    result.append({target}(order))")
                elif rng.random() < 0.5:
                    imports.add(f"from shop import {target_module}")
                    lines.append(f"    result.append({target_module}.{target}(order))")
                else:
                    imports.add(f"from shop.{target_module} import {target}")
                    lines.append(f"    result.append({target}(order))")
            lines.append("    return result")
            bodies.append("\n".join(lines))
            local.append(name)
        header = [f'"""{module.capitalize()}."""', ""] + sorted(imports)
        (root / f"{module}.py").write_text("\n".join(header) + "\n\n\n" + "\n\n\n".join(bodies) + "\n",
                                           encoding="utf-8")
        available += [(module, name) for name in names]


if __name__ == "__main__":
    main()
