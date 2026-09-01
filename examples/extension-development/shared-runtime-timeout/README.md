# shared-runtime-timeout

This example verifies timeout interruption in phase five of the extension shared runtime. The `spin` call in `module.wat` enters an infinite loop, while the `answer` call returns `42`.

Verification goals:

- `runtime.mode` is set to `shared`.
- When `call_timeout_ms` expires, the host interrupts the target worker.
- The worker is recreated after the timeout, and subsequent `answer` calls still return successfully.

This directory retains the WAT, manifest, and bindings so the example can later be packaged as a `.bts` file or adapted into a SQLite shared runtime example.
