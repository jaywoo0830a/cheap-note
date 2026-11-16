2026년 11월 기준으로 네 가지 플랫폼의 Rust 런타임 성능 극한 최적화 옵션을 정리했습니다. 이 시점의 핵심 변화는 **LLVM 22 기반 Rust 1.95+** 가 보편화되면서 **Apple M5/A19**까지 `target-cpu`로 공식 지정할 수 있게 되었고, **BOLT는 여전히 Linux/ELF 전용**이라 Windows와 macOS에서는 **PGO만** 사용할 수 있다는 점입니다.

---

## 🪟 Windows 11 · Intel

### `target-cpu` 선택
2026년 11월 기준 주력은 **Arrow Lake(Core Ultra 200S)** 와 **Lunar Lake(Core Ultra 200V)** 입니다. Rust 1.95+의 LLVM 22는 `arrowlake`와 `lunarlake`를 인식하지만, 툴체인에 따라 아직 `alderlake`로 폴백되는 경우가 있습니다. **가장 안전하게 최대 성능을 내려면 `native`를 사용**하는 것이 좋습니다.

```bash
# 빌드 머신 = 실행 머신일 때 (권장)
RUSTFLAGS="-C target-cpu=native -C target-feature=+avx2,+fma" cargo build --release
```

배포 대상 CPU를 특정하고 싶다면:
```bash
RUSTFLAGS="-C target-cpu=arrowlake -C target-feature=+avx2,+fma" cargo build --release
```
Arrow Lake는 AVX-512를 지원하지 않으므로 `+avx512f` 등을 추가하면 `Illegal Instruction`이 발생합니다.

### 링커 및 프로필
Windows MSVC 타겟(`x86_64-pc-windows-msvc`)에서는 **LLD**를 사용할 수 있습니다. `Cargo.toml`에 `panic = "abort"`, `lto = "fat"`, `codegen-units = 1`을 설정하세요.

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
```

### PGO / BOLT
**BOLT는 Windows에서 사용할 수 없습니다**. **PGO만** 적용 가능하며, `cargo-pgo`로 프로파일 수집 → 최적화 빌드 순으로 진행합니다.

```bash
cargo install cargo-pgo
cargo pgo instrument build --release
./target/release/your_app < representative_workload
cargo pgo optimize build --release
```

---

## 🪟 Windows 11 · AMD

### `target-cpu` 선택
Zen 5 기반 CPU(Ryzen 9000, EPYC 9005)는 **`znver5`** 로 지정합니다. `znver5`는 AVX-512를 포함한 Zen 5의 전체 명령어 세트를 활성화합니다.

```bash
RUSTFLAGS="-C target-cpu=znver5 -C link-arg=-fuse-ld=lld" cargo build --release
```

Zen 4 이하라면 `znver4`(또는 `znver3`)를 사용하세요. `native`는 빌드 머신과 실행 머신이 같을 때만 사용하는 것이 안전합니다.

### 링커 및 프로필
Intel과 동일하게 **LLD**를 사용하고, 프로필에 `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`를 설정합니다.

### PGO / BOLT
**BOLT는 Windows에서 불가**합니다. **PGO만** 적용하세요. AMD Zen 아키텍처는 분기 예측 특성상 PGO의 효과가 큰 편입니다.

```bash
cargo pgo instrument build --release
./target/release/your_app < workload
cargo pgo optimize build --release
```

---

## 🍎 Mac · Apple Silicon (M 시리즈)

### `target-cpu` 선택
2026년 11월 기준 최신 칩은 **M5**이며, LLVM 22에 `apple-m5`가 공식 정의되어 있습니다. Rust 1.95+를 사용한다면 **`apple-m5`를 직접 지정**할 수 있습니다. M4 이하라면 `apple-m4` 또는 `native`를 사용하세요.

```bash
# M5 (Rust 1.95+, LLVM 22)
RUSTFLAGS="-C target-cpu=apple-m5 -C target-feature=+aes,+sha2,+sha3,+fp16" \
cargo build --release
```

`+fp16`은 반정밀도 부동소수점을, `+aes`/`+sha2`/`+sha3`는 암호화 하드웨어 가속을 활성화합니다. M 시리즈는 NEON(ASIMD)을 기본 지원하므로 별도 지정이 필요 없습니다.

### 링커 및 프로필
macOS에서는 시스템 링커(`ld64`)가 기본이지만, **`ld64.lld`** 를 사용하면 링크 시간이 단축됩니다. `panic = "abort"`와 `lto = "fat"`를 적용하세요.

```toml
[profile.release]
opt-level = 3
lto = "fat"
codegen-units = 1
panic = "abort"
```

### PGO / BOLT
**BOLT는 macOS(Mach-O)에서 사용할 수 없습니다** . **PGO만** 적용하세요. Apple Silicon은 PGO 프로파일이 아키텍처에 종속되므로, 대상 기기와 동일한 칩에서 프로파일을 수집해야 합니다.

```bash
cargo pgo instrument build --release --target aarch64-apple-darwin
./target/aarch64-apple-darwin/release/your_app < workload
cargo pgo optimize build --release --target aarch64-apple-darwin
```

---

## 📱 Mac · Apple 모바일 (A 시리즈)

### `target-cpu` 선택
iOS 기기용 빌드는 **`aarch64-apple-ios`** 타겟을 사용합니다. 2026년 11월 기준 최신 칩은 **A19**(아이폰 17 시리즈)이며, LLVM 22에 `apple-a19`가 정의되어 있습니다. A18 이하라면 `apple-a18`을 사용하세요.

```bash
# A19 (Rust 1.95+, LLVM 22)
RUSTFLAGS="-C target-cpu=apple-a19 -C target-feature=+aes,+sha2,+sha3,+fp16" \
cargo build --release --target aarch64-apple-ios
```

`target-cpu`를 지정하지 않으면 `aarch64-apple-ios`의 기본값은 여전히 `apple-a7`(A7 이후 전체 호환)이므로, **A19/A18 전용 앱이라면 반드시 명시적으로 지정**해야 성능 이득을 볼 수 있습니다.

### 링커 및 프로필
iOS도 Mach-O이므로 시스템 링커 또는 `ld64.lld`를 사용합니다. `panic = "abort"`와 `lto = "fat"`를 적용하세요.

### PGO / BOLT
**BOLT는 iOS에서 불가**합니다. **PGO만** 가능하며, 실제 기기(또는 시뮬레이터)에서 프로파일을 수집해야 합니다.

```bash
cargo pgo instrument build --release --target aarch64-apple-ios
# Xcode를 통해 앱을 기기에 설치하고 대표 워크로드 실행
cargo pgo optimize build --release --target aarch64-apple-ios
```

---

## 💎 네 플랫폼 요약

| 플랫폼 | 권장 `target-cpu` | 핵심 `target-feature` | PGO | BOLT | 링커 |
|---|---|---|---|---|---|
| **Win11 · Intel** | `native` (또는 `arrowlake`) | `+avx2,+fma` | ✅ | ❌ | LLD |
| **Win11 · AMD** | `znver5` | (CPU 모델에 내장) | ✅ | ❌ | LLD |
| **Mac · M 시리즈** | `apple-m5` (또는 `native`) | `+aes,+sha2,+sha3,+fp16` | ✅ | ❌ | `ld64.lld` |
| **Mac · A 시리즈** | `apple-a19` (또는 `apple-a18`) | `+aes,+sha2,+sha3,+fp16` | ✅ | ❌ | `ld64.lld` |

공통적으로 `Cargo.toml`의 `[profile.release]`에 **`opt-level = 3`, `lto = "fat"`, `codegen-units = 1`, `panic = "abort"`** 를 설정하고, 빌드 후 **`cargo pgo optimize`** 로 PGO를 적용하는 것이 2026년 11월 기준 Windows와 macOS에서 도달할 수 있는 실질적인 성능 상한입니다. BOLT가 필요하다면 Linux/ELF 환경으로 빌드 파이프라인을 분리해야 합니다.