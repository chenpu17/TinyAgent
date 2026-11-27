"""Descriptive statistics over sequences of numbers."""


def mean(values):
    return sum(values) / len(values)


def median(values):
    ordered = sorted(values)
    middle = len(ordered) // 2
    if len(ordered) % 2:
        return ordered[middle]
    return (ordered[middle - 1] + ordered[middle]) / 2
