import unittest

from tally import total


class TotalTest(unittest.TestCase):
    def test_empty(self):
        self.assertEqual(total([]), 0)

    def test_whole_dollars(self):
        self.assertEqual(total([1, 2]), 3)

    def test_cents(self):
        self.assertEqual(total([0.29, 0.1]), 0.39)


if __name__ == '__main__':
    unittest.main()
