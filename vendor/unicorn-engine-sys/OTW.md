# Unicorn Engine (sys) for OTW

The `unicorn-engine-sys` 2.1.5 crate from crates.io, with two changes so that
several Unicorn instances can run threads of one guest process at once over
shared host memory:

- `qemu/include/exec/exec-all.h`, `uc.c`: `curr_cflags()` returns
  `CF_PARALLEL` once `otw_set_parallel_cpus(1)` was called, so locked
  instructions (`lock cmpxchg`, `lock xadd`, ...) are translated to host atomic
  operations instead of plain loads and stores.
- `qemu/target/i386/mem_helper.c`: `cmpxchg16b` without 128-bit host atomics
  falls back to the unlocked helper instead of an atomic-step exit Unicorn does
  not implement.

- `qemu/accel/tcg/atomic_template.h`: the exchange helpers (`xchg`, and so
  every `xchg` with memory) and the min/max read-modify-write helpers use host
  atomics; Unicorn had a plain load and store, so two CPUs could both take a
  lock built on `xchg`, or one CPU's store could be lost under another's.

Licensed as the original (GPL-2.0 with LGPL parts; see COPYING).
