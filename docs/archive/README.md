# Archived patches

Shelved work — not a queue of pending patches. Each file here is a
full diff that was built, tested, and set aside rather than committed.
Revive with `git apply docs/archive/<file>.patch`.

## Index

- `fix2-ttbr0-always-write.patch` — Always write TTBR0 on context
  switch (EMPTY_USER_L0 for kernel threads), with conditional
  TLB-flush only on kernel→user transitions. Shelved in favor of a
  directional fix: remove the kernel's dependency on lower-half VAs
  entirely (see "Kernel identity map in user TTBR0" in
  `docs/tech-debt.md`). Boot verified: 3/3 reach `uart-driver:
  server ready` in <5s, zero faults.
