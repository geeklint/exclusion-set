[![Tests](https://github.com/geeklint/typeid-set/actions/workflows/rust.yml/badge.svg)](https://github.com/geeklint/typeid-set/actions/workflows/rust.yml)
[![codecov](https://codecov.io/gh/geeklint/typeid-set/branch/main/graph/badge.svg?token=8HZZXB7MUL)](https://codecov.io/gh/geeklint/typeid-set)

A lock-free concurrent set that stores `TypeId`s.

Tested with [loom](https://github.com/tokio-rs/loom).

This source code can be used as a template for a similar data structure for any
type which implements `Eq`.