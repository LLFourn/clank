# Recording book demonstrations

This optional authoring toolchain uses Python 3 to drive and verify terminal
recordings. It is separate from the Rust test suite and release checks, which
do not require Python. Existing recordings can be served without Python.

Run `python3 scripts/book/record.py --help` for recording options. The recorder's
own tests run with `python3 -m unittest scripts/book/test_record.py`.
