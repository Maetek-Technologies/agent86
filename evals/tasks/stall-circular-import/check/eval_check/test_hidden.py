import ast
import pathlib
import subprocess
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


def defined(module):
    tree = ast.parse((ROOT / "shop" / f"{module}.py").read_text(encoding="utf-8"))
    return {node.name for node in tree.body if isinstance(node, (ast.FunctionDef, ast.ClassDef))}


def run(code):
    return subprocess.run([sys.executable, "-c", code], cwd=ROOT, capture_output=True, text=True, timeout=60)


class Hidden(unittest.TestCase):
    def test_nothing_moved_or_removed(self):
        self.assertEqual(defined("models"), {"Product", "Basket"})
        self.assertEqual(defined("pricing"), {"line_price", "price_with_tax"})

    def test_imports_in_either_order(self):
        for code in ("import shop.models", "import shop.pricing",
                     "import shop.models, shop.pricing", "import shop.pricing, shop.models",
                     "from shop.pricing import line_price; from shop.models import Product",
                     "from shop.models import Basket; from shop.pricing import price_with_tax"):
            result = run(code)
            self.assertEqual(result.returncode, 0, f"{code}: {result.stderr}")

    def test_behaviour_in_a_fresh_interpreter(self):
        code = (
            "from shop.pricing import line_price, price_with_tax\n"
            "from shop.models import Basket, Product\n"
            "b = Basket(); b.add(Product('a', 2.5), 2); b.add(Product('b', 10.0, taxable=False))\n"
            "print(b.total(), b.total_with_tax(0.2))\n"
            "try:\n    line_price('x', 1)\nexcept TypeError:\n    print('typeerror')\n"
            "try:\n    price_with_tax(3, 0.1)\nexcept TypeError:\n    print('typeerror')\n"
        )
        result = run(code)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.split(), ["15.0", "16.0", "typeerror", "typeerror"])

    def test_the_tests_pass(self):
        result = subprocess.run([sys.executable, "-m", "unittest", "discover", "-s", "tests", "-t", "."],
                                cwd=ROOT, capture_output=True, text=True, timeout=120)
        self.assertEqual(result.returncode, 0, result.stderr)


if __name__ == "__main__":
    unittest.main()
