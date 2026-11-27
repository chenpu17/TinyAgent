# calc

Small, dependency-free numeric helpers. `tests/` is the specification: it
describes the intended behavior and must not be edited. Fix the library.

## Contract

- `div(a, b)` raises `ValueError("division by zero")` when `b` is `0`.
- `mean(values)` raises `ValueError("mean of empty list")` when `values` is empty.
- `median` never mutates its argument.

## Running the tests

```sh
python3 run_tests.py
```

The last line summarizes the run, for example
`tests: 10, failures: 0, errors: 2`. The exit status is `0` only when every
test passes.
