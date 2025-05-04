#!/bin/sh
export HELIX_RUNTIME=$PWD/runtime
target/release/hx --client /tmp/hx "$@"
