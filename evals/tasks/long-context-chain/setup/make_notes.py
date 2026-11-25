"""Write the chain of notes into ./notes, the same every time.

Each note is about 6,000 characters (some 1,500 tokens): the three lines that
matter, then filler. The names give nothing away, so the chain can only be
followed one note at a time.
"""

import pathlib
import random

CHAIN = [
    ("start", "AMBERFOX", 17),
    ("q7k", "GLASSWREN", 42),
    ("m2z", "TINMOTH", 8),
    ("p9w", "BRASSOWL", 31),
    ("r4t", "SILKHARE", 25),
    ("v1x", "COPPERELK", 9),
    ("b6n", "SLATEJAY", 14),
    ("h3c", "IVORYCRAB", 56),
    ("d8f", "FLINTMOLE", 3),
    ("k5s", "OPALNEWT", 27),
    ("w0g", "RUSTLYNX", 11),
    ("e2y", "PEARLMINK", 38),
]

WORDS = (
    "the quarterly report notes that shipping delays affected the northern "
    "warehouse while staff reviewed inventory counts and supplier contracts "
    "during the audit period before the regional office approved new budgets"
).split()


def main():
    notes = pathlib.Path("notes")
    notes.mkdir(exist_ok=True)
    rng = random.Random(86)
    for index, (name, code, value) in enumerate(CHAIN):
        following = f"notes/{CHAIN[index + 1][0]}.txt" if index + 1 < len(CHAIN) else "none"
        lines = []
        while sum(len(line) + 1 for line in lines) < 5_900:
            lines.append(" ".join(rng.choice(WORDS) for _ in range(14)).capitalize() + ".")
        text = f"CODE: {code}\nVALUE: {value}\nNEXT: {following}\n\n" + "\n".join(lines) + "\n"
        (notes / f"{name}.txt").write_text(text, encoding="utf-8")


if __name__ == "__main__":
    main()
