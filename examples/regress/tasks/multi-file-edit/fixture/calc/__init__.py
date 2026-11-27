"""calc: small, dependency-free numeric helpers.

The contract lives in README.md; tests/ pins it.
"""
from .core import add, div, mul, sub
from .stats import mean, median

__all__ = ["add", "div", "mean", "median", "mul", "sub"]
