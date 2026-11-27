#!/usr/bin/env python3
"""Run the test suite and print a fixed-format summary (no timings).

Usage: python3 run_tests.py       (from the repository root)
Exit status is 0 when every test passes, 1 otherwise.
"""
import io
import sys
import unittest


def main():
    suite = unittest.TestLoader().discover("tests")
    result = unittest.TextTestRunner(stream=io.StringIO(), verbosity=0).run(suite)
    for test, _trace in result.errors:
        print(f"ERROR: {test.id()}")
    for test, _trace in result.failures:
        print(f"FAIL: {test.id()}")
    print(f"tests: {result.testsRun}, failures: {len(result.failures)}, errors: {len(result.errors)}")
    return 0 if result.wasSuccessful() else 1


if __name__ == "__main__":
    sys.exit(main())
