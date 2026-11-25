"""Write thirty-odd ordinary modules into ./service, the same every time, so
the few files that matter have to be found rather than seen at a glance."""

import pathlib
import random

AREAS = ["accounts", "audit", "catalog", "exports", "geo", "mail", "media", "search"]
VERBS = ["load", "save", "merge", "render", "check", "build", "sync", "prune"]
NOUNS = ["record", "batch", "entry", "profile", "bundle", "index", "report", "token"]


def module(rng, area):
    lines = [f'"""{area.capitalize()} helpers."""', "", "import logging", "", "log = logging.getLogger(__name__)", ""]
    lines.append(f"MAX_{rng.choice(NOUNS).upper()}S = {rng.choice([10, 20, 50, 100, 200])}")
    lines.append("")
    for _ in range(rng.randint(4, 7)):
        verb, noun = rng.choice(VERBS), rng.choice(NOUNS)
        lines += [
            "",
            f"def {verb}_{noun}(items, limit={rng.choice([5, 10, 25, 60])}):",
            f'    """{verb.capitalize()} each {noun} in ``items``, at most ``limit`` of them."""',
            "    done = []",
            "    for item in items[:limit]:",
            f'        log.debug("{verb} {noun} %s", item)',
            "        done.append(item)",
            "    return done",
        ]
    return "\n".join(lines) + "\n"


def main():
    rng = random.Random(86)
    root = pathlib.Path("service")
    for area in AREAS:
        package = root / area
        package.mkdir(parents=True, exist_ok=True)
        (package / "__init__.py").write_text("", encoding="utf-8")
        for index in range(4):
            name = f"{rng.choice(VERBS)}_{rng.choice(NOUNS)}s_{index}.py"
            (package / name).write_text(module(rng, area), encoding="utf-8")


if __name__ == "__main__":
    main()
