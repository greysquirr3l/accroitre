# Rust 1.78 → 1.94 Catchup Guide

A comprehensive reference for Rust developers catching up from version 1.77 to 1.94 (Mar 2026).

---

## Table of Contents

- [Major Highlights](#major-highlights)
- [Rust 2024 Edition](#rust-2024-edition)
- [Release-by-Release Summary](#release-by-release-summary)
- [Feature Categories](#feature-categories)
  - [Language Features](#language-features)
  - [Async & Concurrency](#async--concurrency)
  - [Type System](#type-system)
  - [Macros & Attributes](#macros--attributes)
  - [Const Evaluation](#const-evaluation)
  - [Platform & Tooling](#platform--tooling)
  - [Standard Library](#standard-library)

---

## Major Highlights

### Rust 2024 Edition (1.85, Feb 2025)

The largest edition change since 2018. Key changes:

- **Async closures**: `async || { }` with `AsyncFn` traits
- **RPIT capture rules**: Return position `impl Trait` captures all in-scope lifetimes by default
- **`gen` keyword reserved** for future generators
- **`unsafe extern` blocks** for FFI declarations
- **Static mut references denied** by default

### Top Features by Impact

| Feature | Version | Impact |
| --------- | --------- | -------- |
| Let chains (`if let && let`) | 1.88 | High (2024 edition only) |
| Async closures | 1.85 | High |
| Trait upcasting | 1.86 | High |
| `LazyCell`/`LazyLock` | 1.80 | High (replaces `lazy_static`/`once_cell`) |
| Naked functions | 1.88 | Medium |
| Precise capturing `use<..>` | 1.82 | Medium |
| MSRV-aware Cargo resolver | 1.84 | Medium |
| `core::error::Error` | 1.81 | Medium (no_std) |
| Anonymous pipes `std::io::pipe()` | 1.87 | Medium |

---

## Rust 2024 Edition

**Enabled with:** `edition = "2024"` in `Cargo.toml` (requires Rust 1.85+)

### Breaking Changes from 2021

1. **RPIT lifetime capture**: `-> impl Trait` now captures *all* in-scope lifetimes by default

   ```rust
   // 2021: Only captures explicitly mentioned lifetimes
   // 2024: Captures 'a automatically
   fn foo<'a>(x: &'a str) -> impl Sized { x }
   ```

   Use `use<..>` for explicit control: `-> impl Sized + use<>` (no captures)

2. **`unsafe extern` blocks**: FFI declarations must be in `unsafe extern` blocks

   ```rust
   unsafe extern "C" {
       fn external_fn();
   }
   ```

3. **`gen` keyword reserved**: Cannot use `gen` as an identifier

4. **Static mut borrow denied**: `&mut STATIC_MUT` is now a hard error

   ```rust
   // Error in 2024:
   static mut X: i32 = 0;
   let r = unsafe { &mut X }; // ❌
   // Use raw pointers or synchronization instead
   ```

5. **`if let` chains**: Enabled only in 2024 edition

   ```rust
   if let Some(x) = opt && let Some(y) = x.inner && y > 0 {
       // ...
   }
   ```

6. **Lifetime elision changes**: Some previously allowed elisions now require explicit annotation

### New in 2024 Edition

- `async || { }` closures with `AsyncFn`, `AsyncFnMut`, `AsyncFnOnce` traits
- `#[diagnostic::do_not_recommend]` for better trait error messages
- Expanded macro hygiene

---

## Release-by-Release Summary

### Rust 1.78 (May 2024)

- `#[diagnostic::on_unimplemented]` - Custom trait error messages
- Unsafe precondition assertions in debug builds
- Deterministic realignment for over-aligned types

### Rust 1.79 (Jun 2024)

- **Inline const expressions**: `const { expr }` anywhere

  ```rust
  let x = const { 1 + 2 };
  ```

- **Bounds in associated type position**: `T: Trait<Assoc: Bound>`
- Automatic temporary lifetime extension in match/if

### Rust 1.80 (Jul 2024)

- **`LazyCell` / `LazyLock`** stabilized (replaces `lazy_static!` / `once_cell`)

  ```rust
  use std::sync::LazyLock;
  static CONFIG: LazyLock<Config> = LazyLock::new(|| load_config());
  ```

- **Checked `cfg`**: `--check-cfg` for validating cfg names/values
- **Exclusive ranges in patterns**: `0..10`, `..5`

  ```rust
  match x {
      ..0 => negative(),
      0..10 => small(),
      10.. => large(),
  }
  ```

### Rust 1.81 (Sep 2024)

- **`core::error::Error`** - Error trait in `core` (no_std support!)
- **New sort implementations** - Faster, panic-safe sorting
- **`#[expect(lint)]`** - Like `#[allow]` but warns if lint doesn't fire
- **Lint reasons**: `#[allow(unused, reason = "temporary")]`

### Rust 1.82 (Oct 2024)

- **`cargo info`** command
- **`aarch64-apple-darwin`** promoted to Tier 1
- **Precise capturing `use<..>`**: Control RPIT lifetime captures

  ```rust
  fn foo<'a, T>(x: &'a (), y: T) -> impl Sized + use<'a, T> { (x, y) }
  ```

- **Raw pointer syntax**: `&raw const expr`, `&raw mut expr`
- **`unsafe extern` blocks** (preview, required in 2024)
- **`unsafe` attributes**: `#[unsafe(no_mangle)]`
- **Empty type patterns**: Can omit patterns for uninhabited types
- **NaN semantics**: `f32::min/max` are now `const` with defined NaN behavior

### Rust 1.83 (Nov 2024)

- **Major const expansion**:
  - Mutable references in const
  - References to statics in const
  - Many APIs const-stabilized (`Cell::get`, `MaybeUninit::*`, etc.)

### Rust 1.84 (Jan 2025)

- **MSRV-aware Cargo resolver**: Automatically considers minimum supported Rust version

  ```toml
  [package]
  rust-version = "1.75"
  # Cargo will prefer deps compatible with 1.75
  ```

- **New trait solver** for coherence checking
- **Strict provenance APIs**: `ptr::without_provenance`, `ptr::with_exposed_provenance`

### Rust 1.85 (Feb 2025) — **2024 EDITION**

- **Rust 2024 Edition released**
- **Async closures**: `async || { }` with `AsyncFn*` traits

  ```rust
  let closure = async || {
      some_async_fn().await
  };
  ```

- **`#[diagnostic::do_not_recommend]`**: Hide trait impls from error suggestions
- Many 2024 edition changes (see above)

### Rust 1.86 (Apr 2025)

- **Trait upcasting**: `dyn Trait` → `dyn Supertrait` coercion

  ```rust
  trait Animal { fn name(&self) -> &str; }
  trait Dog: Animal { fn bark(&self); }
  
  fn use_animal(dog: &dyn Dog) {
      let animal: &dyn Animal = dog; // Now works!
  }
  ```

- **`get_disjoint_mut`**: Mutable access to multiple disjoint indices

  ```rust
  let [a, b] = slice.get_disjoint_mut([0, 5]).unwrap();
  ```

- **Safe `#[target_feature]`**: Can mark safe fns with target features
- **Debug null pointer assertions**: Debug builds check null derefs
- **`missing_abi` lint** now warn-by-default

### Rust 1.87 (May 2025) — **10 Years of Rust!**

- **Anonymous pipes**: `std::io::pipe()`

  ```rust
  let (reader, writer) = std::io::pipe()?;
  ```

- **Safe arch intrinsics**: Many SIMD intrinsics now safe
- **`asm!` label operand**: Jump to Rust code from inline asm
- **Precise capturing in trait definitions**: `use<..>` in trait methods
- **`Vec::extract_if`** stabilized

### Rust 1.88 (Jun 2025)

- **Let chains** (2024 edition only)

  ```rust
  if let Some(x) = opt && let Ok(y) = x.parse::<i32>() && y > 0 {
      println!("{y}");
  }
  ```

- **Naked functions**: `#[unsafe(naked)]` with `naked_asm!`

  ```rust
  #[unsafe(naked)]
  extern "C" fn add(a: i32, b: i32) -> i32 {
      naked_asm!("add eax, edi, esi", "ret")
  }
  ```

- **Boolean `cfg`**: `cfg(true)`, `cfg(false)`
- **Cargo automatic cache cleaning**: Auto-removes old build artifacts

### Rust 1.89 (Aug 2025)

- **Inferred const generic args**: `[T; _]` infers length

  ```rust
  fn zeros<const N: usize>() -> [i32; N] {
      [0; _] // _ infers N
  }
  ```

- **`mismatched_lifetime_syntaxes` lint**: Warns on inconsistent lifetime notation
- **File locking APIs**: `File::lock()`, `File::try_lock()`, `File::unlock()`
- **`Result::flatten`** stabilized
- **x86_64-apple-darwin** demoted to Tier 2 (Apple Silicon transition)
- **Cross-compiled doctests** now run
- **`i128`/`u128` in `extern "C"`**: No longer triggers ctypes lint

### Rust 1.90 (Sep 2025)

- **LLD default linker** on `x86_64-unknown-linux-gnu`
- **`cargo publish --workspace`**: Publish all workspace crates at once
- **x86_64-apple-darwin** officially Tier 2
- **Const `f32`/`f64` rounding**: `floor`, `ceil`, `trunc`, `round` now const

### Rust 1.91 (Oct 2025)

- **`aarch64-pc-windows-msvc`** promoted to Tier 1
- **Dangling pointer lint**: Warns on returning raw pointers to locals
- **`BTreeMap::extract_if`**, **`BTreeSet::extract_if`** stabilized
- **`Duration::from_hours`**, **`Duration::from_mins`**
- **`Path::file_prefix`**
- **Strict integer ops**: `strict_add`, `strict_mul`, etc. (panic on overflow)
- **Carrying/borrowing math**: `carrying_add`, `borrowing_sub`, `carrying_mul`
- **`str::floor_char_boundary`**, **`str::ceil_char_boundary`**

### Rust 1.92 (Dec 2025)

- **Never type lints deny-by-default**: Preparation for `!` stabilization
- **`unused_must_use` improvement**: No warning for `Result<(), Infallible>`
- **Unwind tables with `-Cpanic=abort`**: Backtraces work again
- **`RwLockWriteGuard::downgrade`**: Downgrade write lock to read lock
- **Zeroed allocation**: `Box::new_zeroed()`, `Arc::new_zeroed()`, `Rc::new_zeroed()`

### Rust 1.93 (Jan 2026)

- **musl updated to 1.2.5**: Better DNS resolution, breaking change for old libc
- **Global allocator can use TLS**: Thread-local storage in custom allocators
- **`cfg` on `asm!` lines**: Conditional assembly statements

  ```rust
  asm!(
      "nop",
      #[cfg(target_feature = "sse2")]
      "sse2_instruction",
  );
  ```

- **`MaybeUninit` slice methods**: `assume_init_ref`, `assume_init_mut`
- **`Vec::into_raw_parts`**, **`String::into_raw_parts`**
- **`std::fmt::from_fn`**: Create `Display` impl from closure
- **`VecDeque::pop_front_if`**, **`VecDeque::pop_back_if`**

### Rust 1.94 (Mar 2026)

- **`<[T]>::array_windows`**: Sliding window iterator over slices

  ```rust
  let data = [1, 2, 3, 4, 5];
  for [a, b] in data.array_windows() {
      println!("{a} {b}");
  }
  ```

- **`LazyCell::get` / `LazyLock::get`**: Non-forcing access to lazy values

  ```rust
  use std::sync::LazyLock;
  static CONFIG: LazyLock<String> = LazyLock::new(|| "loaded".into());
  // Returns None if not yet initialized:
  let maybe = LazyLock::get(&CONFIG);
  ```

- **`Peekable::next_if_map`**: Conditionally map-and-consume the next element
- **`<[T]>::element_offset`**: Get the index of a reference within a slice
- **`BinaryHeap<T>` relaxed bounds**: Some methods no longer require `T: Ord`
- **`f32::consts::EULER_GAMMA` / `f64::consts::EULER_GAMMA`**: Euler-Mascheroni constant
- **`f32::consts::GOLDEN_RATIO` / `f64::consts::GOLDEN_RATIO`**: Golden ratio constant
- **`impl TryFrom<char> for usize`**
- **Cargo `include` config key** stabilized: Load additional config files from `config.toml`
- **Cargo TOML v1.1**: Manifests and config now parse TOML v1.1 syntax
- **Cargo `pubtime` field**: Registry index records when a version was published
- **`f32::mul_add` / `f64::mul_add`** now `const`
- **Unicode 17** update
- **`dead_code` lint inheritance**: Impls and impl items inherit lint level from corresponding traits
- **`unused_visibilities` lint** (warn-by-default): Warns on `pub const _` declarations
- **`annotate-snippets` for error emission**: Compiler diagnostic output engine replaced

#### Compatibility Notes (1.94)

- Freely casting lifetime bounds of `dyn` types is now forbidden
- Closure capture behavior around patterns changed for consistency
- Standard library macros imported via prelude instead of `#[macro_use]` — glob imports of same-name macros now require disambiguation
- Codegen attributes on body-free trait methods trigger a future-compat warning
- `SystemTime::checked_sub_duration` returns `None` for pre-Windows-epoch times on Windows

### Rust 1.94.1 (Mar 2026) — patch release

- **Fix `std::thread::spawn` on `wasm32-wasip1-threads`**
- **Remove new methods added to `std::os::windows::fs::OpenOptionsExt`**: The new methods were unstable, but the trait is not sealed and so cannot be extended with non-default methods
- **Clippy**: fix ICE in `match_same_arms`
- **Cargo**: update `tar` to 0.4.45 — resolves CVE-2026-33055 and CVE-2026-33056 (crates.io users unaffected)
- **Cargo**: fix certificate validation errors on FreeBSD

---

## Feature Categories

### Language Features

| Feature | Version | Example |
| --------- | --------- | --------- |
| Inline const | 1.79 | `let x = const { expr };` |
| Let chains | 1.88 (2024) | `if let Some(x) = a && let Some(y) = b { }` |
| Exclusive range patterns | 1.80 | `match x { ..0 => {}, 0..10 => {} }` |
| Async closures | 1.85 (2024) | `async \|\| { fut.await }` |
| Trait upcasting | 1.86 | `&dyn Sub` → `&dyn Super` |
| Naked functions | 1.88 | `#[unsafe(naked)] fn f() { naked_asm!(...) }` |
| Precise capturing | 1.82 | `-> impl Trait + use<'a, T>` |
| Raw pointer syntax | 1.82 | `&raw const x`, `&raw mut x` |

### Async & Concurrency

| Feature | Version | Notes |
| --------- | --------- | ------- |
| Async closures | 1.85 | `AsyncFn`, `AsyncFnMut`, `AsyncFnOnce` traits |
| `LazyLock` | 1.80 | Thread-safe lazy initialization |
| `LazyCell` | 1.80 | Single-threaded lazy initialization |
| `LazyLock::get` / `LazyCell::get` | 1.94 | Non-forcing access to lazy value |
| `RwLockWriteGuard::downgrade` | 1.92 | Write → Read lock downgrade |

### Type System

| Feature | Version | Notes |
| --------- | --------- | ------- |
| Bounds in assoc type position | 1.79 | `T: Trait<Assoc: Bound>` |
| Trait upcasting | 1.86 | Coerce subtrait to supertrait |
| Never type lints | 1.92 | Preparing for `!` stabilization |
| `#[diagnostic::do_not_recommend]` | 1.85 | Hide impls from suggestions |
| `#[diagnostic::on_unimplemented]` | 1.78 | Custom trait error messages |

### Macros & Attributes

| Feature | Version | Notes |
| --------- | --------- | ------- |
| `#[expect(lint)]` | 1.81 | Warn if expected lint doesn't fire |
| `unsafe` attributes | 1.82 | `#[unsafe(no_mangle)]` |
| `cfg(true)`/`cfg(false)` | 1.88 | Boolean cfg values |
| `cfg` on asm lines | 1.93 | Conditional assembly |
| Lint reasons | 1.81 | `#[allow(x, reason = "...")]` |

### Const Evaluation

Massive expansion in const capabilities from 1.79–1.93:

| Capability | Version |
| ------------ | --------- |
| Inline const `const { }` | 1.79 |
| Mutable references in const | 1.83 |
| References to statics in const | 1.83 |
| `f32`/`f64` rounding (const) | 1.90 |
| Many `MaybeUninit` methods | 1.83+ |
| `Cell::get` (const) | 1.83 |
| `TypeId::of` (const) | 1.91 |
| `f{32,64}::mul_add` (const) | 1.94 |

### Platform & Tooling

#### Platform Tier Changes

| Target | Change | Version |
| -------- | -------- | --------- |
| `aarch64-apple-darwin` | → Tier 1 | 1.82 |
| `aarch64-pc-windows-msvc` | → Tier 1 | 1.91 |
| `x86_64-apple-darwin` | → Tier 2 | 1.90 |

#### Cargo Improvements

| Feature | Version | Notes |
| --------- | --------- | ------- |
| `cargo info` | 1.82 | View crate info |
| MSRV-aware resolver | 1.84 | Respects `rust-version` |
| `cargo publish --workspace` | 1.90 | Publish all crates |
| Auto cache cleaning | 1.88 | Removes old artifacts |
| `--check-cfg` | 1.80 | Validate cfg names/values |
| Config `include` key | 1.94 | Load additional config files |
| TOML v1.1 parsing | 1.94 | Manifests support TOML v1.1 |
| `pubtime` registry field | 1.94 | Tracks publish timestamps |

#### Linker Changes

| Change | Version |
| -------- | --------- |
| LLD default on `x86_64-unknown-linux-gnu` | 1.90 |

### Standard Library

#### Collections

| API | Version |
| ----- | --------- |
| `HashMap::get_disjoint_mut` | 1.86 |
| `Vec::extract_if` | 1.87 |
| `BTreeMap::extract_if` | 1.91 |
| `VecDeque::pop_front_if` | 1.93 |
| `<[T]>::array_windows` | 1.94 |
| `Peekable::next_if_map` | 1.94 |

#### I/O

| API | Version |
| ----- | --------- |
| `std::io::pipe()` | 1.87 |
| `File::lock()` / `try_lock()` / `unlock()` | 1.89 |

#### Error Handling

| API | Version |
| ----- | --------- |
| `core::error::Error` | 1.81 |
| `Result::flatten` | 1.89 |

#### Memory

| API | Version |
| ----- | --------- |
| `Box::new_zeroed()` | 1.92 |
| `Arc::new_zeroed()` | 1.92 |
| `Vec::into_raw_parts` | 1.93 |
| `MaybeUninit` slice methods | 1.93 |

#### Numerics

| API | Version |
| ----- | --------- |
| `strict_add`, `strict_mul`, etc. | 1.91 |
| `carrying_add`, `borrowing_sub` | 1.91 |
| `NonZero<char>` | 1.89 |
| `f{32,64}::consts::EULER_GAMMA` | 1.94 |
| `f{32,64}::consts::GOLDEN_RATIO` | 1.94 |

#### Time & Duration

| API | Version |
| ----- | --------- |
| `Duration::from_hours` | 1.91 |
| `Duration::from_mins` | 1.91 |
| `Duration::from_nanos_u128` | 1.93 |

#### Paths

| API | Version |
| ----- | --------- |
| `Path::file_prefix` | 1.91 |
| `PathBuf::add_extension` | 1.91 |

#### Formatting

| API | Version |
| ----- | --------- |
| `std::fmt::from_fn` | 1.93 |
| `<[T]>::element_offset` | 1.94 |

#### Provenance (Strict)

| API | Version |
| ----- | --------- |
| `ptr::without_provenance` | 1.84 |
| `ptr::with_exposed_provenance` | 1.84 |

---

## Migration Checklist

When upgrading from Rust 1.77:

### Must Do

- [ ] Update `Cargo.toml` to `edition = "2024"` when ready
- [ ] Replace `lazy_static!` with `LazyLock`/`LazyCell`
- [ ] Review `static mut` usage (denied in 2024)
- [ ] Update FFI blocks to `unsafe extern`
- [ ] Check for `gen` identifier conflicts

### Should Do

- [ ] Enable MSRV-aware resolver: `resolver.incompatible-rust-versions = "fallback"`
- [ ] Use `#[expect(lint)]` instead of `#[allow(lint)]` where appropriate
- [ ] Add lint reasons for clarity
- [ ] Consider `--check-cfg` for cfg validation

### Nice to Have

- [ ] Use async closures for cleaner async code
- [ ] Adopt `use<..>` for explicit RPIT captures
- [ ] Replace manual impl Trait lifetime dance with trait upcasting
- [ ] Use `get_disjoint_mut` for multiple mutable borrows

---

## Generated: March 2026 | Covers Rust 1.78–1.94
