import unittest

from calc import add, div, mean, median, mul, sub


class TestCore(unittest.TestCase):
    def test_add(self):
        self.assertEqual(add(2, 3), 5)

    def test_sub(self):
        self.assertEqual(sub(2, 3), -1)

    def test_mul(self):
        self.assertEqual(mul(4, 5), 20)

    def test_div(self):
        self.assertEqual(div(7, 2), 3.5)

    def test_div_by_zero_is_a_value_error(self):
        with self.assertRaises(ValueError) as caught:
            div(1, 0)
        self.assertEqual(str(caught.exception), "division by zero")


class TestStats(unittest.TestCase):
    def test_mean(self):
        self.assertEqual(mean([1, 2, 3, 4]), 2.5)

    def test_median_odd(self):
        self.assertEqual(median([3, 1, 2]), 2)

    def test_median_even(self):
        self.assertEqual(median([4, 1, 3, 2]), 2.5)

    def test_mean_of_empty_is_a_value_error(self):
        with self.assertRaises(ValueError) as caught:
            mean([])
        self.assertEqual(str(caught.exception), "mean of empty list")

    def test_median_preserves_input(self):
        values = [3, 1, 2]
        median(values)
        self.assertEqual(values, [3, 1, 2])
