# Local modifications

This directory contains the source of `permutation_iterator` 0.1.2 under its
original Apache-2.0 license.

CrabCode changes the crate's `rand` dependency from the vulnerable 0.7 line to
the API-compatible, patched 0.8.6-or-newer line. The permutation algorithm is
unchanged.
