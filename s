#!/bin/sh
rm /tmp/hx
export HELIX_RUNTIME=$PWD/runtime
RUST_BACKTRACE=1 target/release/hx --server /tmp/hx
