import ast
import pathlib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
TARGET = "charge_card"


def call_graph():
    """Every function in shop/ and the functions it calls, by name (names are
    unique across the package), resolving `from x import f as g` and calls
    through imported modules."""
    edges = {}
    for path in sorted((ROOT / "shop").rglob("*.py")):
        tree = ast.parse(path.read_text(encoding="utf-8"))
        aliases = {}
        for node in tree.body:
            if isinstance(node, ast.ImportFrom):
                for name in node.names:
                    aliases[name.asname or name.name] = name.name
        for node in tree.body:
            if not isinstance(node, ast.FunctionDef):
                continue
            called = set()
            for inner in ast.walk(node):
                if isinstance(inner, ast.Call):
                    func = inner.func
                    if isinstance(func, ast.Name):
                        called.add(aliases.get(func.id, func.id))
                    elif isinstance(func, ast.Attribute):
                        called.add(func.attr)
            edges[node.name] = called
    return edges


def callers_of(target, edges):
    reaching = set()
    changed = True
    while changed:
        changed = False
        for name, called in edges.items():
            if name not in reaching and name != target and (target in called or called & reaching):
                reaching.add(name)
                changed = True
    return reaching


class Hidden(unittest.TestCase):
    def test_the_list(self):
        expected = callers_of(TARGET, call_graph())
        self.assertGreater(len(expected), 5)
        path = ROOT / "callers.txt"
        self.assertTrue(path.is_file(), "callers.txt was not written")
        lines = [line.strip() for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]
        got = {line.split(".")[-1].split("(")[0].strip("`*- ") for line in lines}
        self.assertEqual(sorted(got - expected), [], "listed but not callers")
        self.assertEqual(sorted(expected - got), [], "callers missing from the list")


if __name__ == "__main__":
    unittest.main()
