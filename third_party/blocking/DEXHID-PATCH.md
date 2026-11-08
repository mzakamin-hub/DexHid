# blocking 1.7.0 with one DexHid change

Source: crates.io `blocking` 1.7.0 (smol-rs, Apache-2.0 OR MIT, licence files kept in this directory).

Change (src/lib.rs, `Executor::main_loop`): an idle pool thread exits when its 500 ms wait times out with an empty
queue. Upstream 1.7 only lets a thread exit when the pool has more threads than its limit (500 by default), so the
threads zbus starts for its socket connect / close stay forever and each wakes every 500 ms (measured in the DexHid
daemon: ~4 wake-ups / s while idle). With the change the pool is empty when nothing runs; a later task starts a new
thread (as upstream does when the pool is empty).
