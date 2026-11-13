# RYS engine performance and roadmap

## Current status

RYS currently compiles source to bytecode and executes it in a stack-based VM. It has a bytecode verifier, persistent bytecode cache, integer arithmetic fast paths, constant folding and fused comparisons/branches. It **does not have a JIT or an ahead-of-time native compiler**. The `.rysc` cache is bytecode, not native machine code.

The VM changes in this revision reuse the built-in call argument buffer and index strings without first collecting all characters into a temporary array. ASCII indexes take a direct byte path; Unicode indexes remain based on Unicode scalar values, including negative indexes. No new dependencies are required.

## Reproduce the benchmarks

Build two revisions with the same Rust toolchain and optimization settings, saving each binary outside the repository. For example, build the baseline in a separate git worktree, and build the candidate with `cargo build --release --locked`.

```sh
python3 benchmarks/compare.py /path/to/baseline target/release/rys --runs 16
python3 benchmarks/run.py target/release/rys --runs 9 --cache off
python3 benchmarks/run.py target/release/rys --runs 9 --cache warm
```

The comparison alternates execution order, checks program output and reports medians and ranges. These are **end-to-end wall-clock** times, including startup, file loading, compiling or cache loading, verification and execution; they are not isolated VM instruction timings. `run.py` uses a temporary cache directory and performs a warm-up. Cache-off tests set `RYS_NO_CACHE`; warm-cache tests populate the cache before measuring. The workloads are small microbenchmarks and do not represent all programs.

### Example measurements

Measured in a shared Linux x86-64 sandbox on an Intel Xeon 2.60 GHz, Rust 1.100.0, release builds; baseline commit `0f6bc4c`, 16 measured runs per version in alternating order, cache disabled. Times are medians; ranges show observed minimum–maximum. Results are noisy and not a promise of typical speedup.

| Workload | Baseline | Candidate | Baseline / candidate | Ranges (ms) |
| --- | ---: | ---: | ---: | --- |
| Integer arithmetic loop | 66.86 ms | 66.35 ms | 1.01× | 57.34–79.00 / 54.03–109.48 |
| Array indexing | 49.53 ms | 50.98 ms | 0.97× | 43.54–66.37 / 45.90–63.02 |
| Built-in calls | 75.17 ms | 50.05 ms | 1.50× | 55.23–88.32 / 43.43–79.26 |
| ASCII string indexing | 91.55 ms | 37.27 ms | 2.46× | 72.67–110.24 / 30.28–59.70 |

A separate nine-run warm-cache check was also performed, but it is not directly comparable to this alternating run. More repetitions on an idle dedicated machine and representative real applications are needed. `perf` and Valgrind were not available in the measurement environment, so these results are **not a CPU sampling profile**. Inspection of bytecode and source suggested the two targeted allocations; it does not establish the proportion of CPU time they consumed.

## Profiling next steps

On Linux, run `cargo build --profile profiling` to build with debug symbols and optimizations, then use `perf` or `cargo flamegraph` with `target/profiling/rys`; OS permissions may restrict sampling. Compare a release-equivalent build for final timing. Use longer runs, record CPU and toolchain details, compare cold and warm startup separately, and test Unicode, recursion, allocation, I/O and real applications. Benchmark both typical and worst-case inputs; do not infer broad performance from a single microbenchmark.

## JIT design (planned, not implemented)

1. Keep the verified VM as the reference implementation. Add opt-in instrumentation for function calls and loop backedges; measure counts without slowing normal execution. Set and tune a hotness threshold, and limit compilation time and code memory.
2. Prototype an optional Cranelift backend for a small, explicitly defined subset of integer-only functions. Compile only verified bytecode, and enter native code only when arguments satisfy type guards. Unsupported functions run in the VM.
3. Preserve RYS semantics exactly: checked integer overflow, truthiness, error line numbers, function arguments, and stack behavior. Initial compilation should reject operations with side effects and avoid mid-function exits; if a guard fails before execution, run the original function in the VM. Never restart after a side effect. More general guards require exact state reconstruction and deoptimization.
4. Differential-test VM versus JIT on generated and hand-written programs, including mixed types, overflow, recursion and failures. Run sanitizers/fuzzing and cross-platform CI. Audit executable memory permissions, lifetime, ABI, resource limits and untrusted input before enabling by default.
5. Benchmark compile time, warm-up, steady-state throughput, memory and fallback frequency. A JIT is only worth shipping if it helps realistic workloads without unacceptable regressions.

Cranelift's `JITModule` emits callable code into memory; it is a backend, not a ready-made RYS runtime. It adds dependencies and requires careful unsafe boundaries. See [Cranelift JIT docs](https://docs.rs/cranelift-jit/latest/cranelift_jit/struct.JITModule.html), [the JIT demo](https://github.com/bytecodealliance/cranelift-jit-demo), and [Cornell's overview of guards and deoptimization](https://www.cs.cornell.edu/courses/cs6120/2025fa/lesson/12/).

## Ahead-of-time compilation (planned, not implemented)

Evaluate AOT after the JIT subset and shared lowering are proven. Cranelift's [ObjectModule](https://docs.rs/cranelift-object) emits native object files, not complete standalone RYS executables. A usable AOT compiler would also need a runtime ABI, linker/toolchain integration, error handling, platform support, and a strategy for dynamic operations. Native artifacts must be treated as executable code, not as untrusted bytecode; see the [Wasmtime precompilation security warning](https://docs.wasmtime.dev/examples-pre-compiling-wasm.html).

## Further reading

- [Virtual-machine abstraction and optimization techniques](https://publications.sba-research.org/publications/bytecode09.pdf): dispatch optimizations vary by workload and interpreter architecture.
- [Rust profiling guidance](https://www.rustfaq.org/en/how-to-profile-rust-code/): profiling optimized builds with symbols and using sampling tools.
