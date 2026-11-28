# COMPILE.md — 빌드 전략 (이 저장소에서 측정한 것만)

이 문서는 **실측 결과**만 적습니다. 아래 측정 환경이 아니면 다른 숫자와 비교하지 마세요.
원본 초안(일반론)은 `git show 82dd868:COMPILE.md`로 볼 수 있었으나, 이 환경에서 틀린 전제가 여럿이라
검증 결과로 대체했습니다.

## 측정 환경

| 항목 | 값 |
|---|---|
| 칩 | **Apple M6** (초안은 M5를 최신이라 했으나 이 머신은 M6) |
| 디스플레이 | 2560×1600, **120 Hz** → `warm`의 하한 = 8.33 ms |
| rustc | **1.99.1** (초안은 1.95+) |
| LLVM | **23.1.1** (초안은 22) |
| 호스트 타겟 | `aarch64-apple-darwin` |

**측정 지표는 `rebuild`입니다.** `warm`은 디스플레이 대기라 잉크와 무관합니다
(`doc/CLI.md` §"`warm` on macOS has a floor"). 방법은 base/PGO **교대 실행** 후 중앙값 + 95% CI.

---

## 1. 지금 적용되어 있는 것

```toml
[profile.release]
opt-level = 3
lto = "thin"
codegen-units = 1
panic = "abort"
strip = "symbols"     # 27.0 MB → 22.3 MB, 성능 변화 없음
```

`strip`은 심볼 4.6 MB를 지웁니다. 벤치 수치에 영향 없음을 확인했습니다 (`82dd868`).

---

## 2. 효과 **있음**: PGO

### `rebuild` @8,182 — **-7.6 %**, 20/20 전부 개선

| | 중앙값 |
|---|---|
| base | 65.05 ms |
| PGO | **60.28 ms** |
| 차이 | **-4.95 ms**, 95% CI **-7.90 .. -4.00** |

CI가 0을 포함하지 않으므로 노이즈가 아닙니다. 페이지별(7회 중앙값):

| strokes | base | PGO | 차이 |
|---|---|---|---|
| 0 | 8.26 | 8.30 | — |
| 29 | 8.76 | 8.86 | — |
| 316 | 14.22 | 13.28 | -6.6 % |
| 1 000 | 20.11 | 19.86 | -1.2 % |
| 8 182 | 77.19 | **72.99** | **-5.4 %** |

**부수 효과: 바이너리가 20.19 MB.** base(22.34 MB)보다 **2 MB 작습니다.**

```sh
# 0) 준비
cargo install cargo-pgo
rustup component add llvm-tools-preview

# 1) 계측 빌드
cargo pgo instrument build -- -p cheap-note-macos

# 2) 대표 워크로드로 프로파일 수집 — 페이지 크기를 골고루 돌려야 한다
B=target/aarch64-apple-darwin/release/cheap-note
for s in 0 29 316 1000 8182; do $B --bench --format json --strokes $s --frames 30 >/dev/null; done

# 3) 최적화 빌드 (llvm-profdata가 PATH에 필요)
export PATH="$HOME/.rustup/toolchains/1.99.1-aarch64-apple-darwin/lib/rustlib/aarch64-apple-darwin/bin:$PATH"
cargo pgo optimize build -- -p cheap-note-macos
```

**프로파일은 칩에 종속됩니다.** M6에서 모은 것을 다른 모델에 배포하면 이득이 사라집니다(손해는 아님).

---

## 3. 효과 **없음** — 다시 시도하지 말 것

### `-C target-cpu=native` — **6 % 느려짐**

반직관적이라 여러 번 확인했습니다: 8회 연속, 15회 중 13회, 최종 20회 중 18회에서 일관되게 느림.

| | 중앙값 (`rebuild` @8,182) |
|---|---|
| base | 69.73 ms |
| `native` | **74.09 ms** |

원인 분해:

| 실험 | 결과 |
|---|---|
| `RAYON_NUM_THREADS=1` | 격차 그대로 → 병렬화 문제 아님 |
| 코드 크기 | `native`가 오히려 작음(13.49 vs 13.71 MB) → 캐시 문제 아님 |
| `-mte` 제거 | 그대로 → `mte` 원인 아님 |
| `-mte,-bti,-i8mm,-bf16` (기본과 동일한 기능 집합) | **여전히 느림** |

마지막 줄이 결정적입니다. 기능 집합을 기본값과 똑같이 맞춰도 `native`를 주면 느립니다.
**`native`의 코드 생성(스케줄링) 자체가 이 워크로드에 불리합니다.** LLVM 23 + M6 조합의 특성.

참고로 기본 타겟은 이미 `aes`, `sha2`, `sha3`, `dotprod`, `fp16`, `lse`, `neon` 등 **29개 기능**을 켭니다.
`native`가 추가하는 것은 `bf16`, `bti`, `i8mm`, `mte` **4개뿐**이고, 그 4개가 이득을 주지 않습니다.

### `lto = "fat"` — 이득 없음, 빌드만 느려짐

| | 크기 | 클린 빌드 | 성능 |
|---|---|---|---|
| `thin` (현재) | 22.34 MB | 72 s | — |
| `fat` | 21.02 MB | **111 s** | 변화 없음 |

### `opt-level = "s"` — **2배 느려짐**

| | 크기 | `rebuild` @8,182 |
|---|---|---|
| `3` | 22.34 MB | 91 ms |
| `"s"` | 20.56 MB | **188 ms** |

테셀레이션 루프가 벡터화를 못 받습니다. 1.8 MB 아끼려고 쓰기 지연을 2배로 만드는 건 손해입니다.

### `ld64.lld` — 이 환경에 없음

`which ld64.lld` → 없음, Homebrew LLVM도 미설치. 링크 시간이 문제가 된 적이 없어 도입할 이유도 없습니다.

### BOLT — macOS(Mach-O)에서 불가

초안이 맞습니다. 검증 대상도 아닙니다.

---

## 4. 권장 전략

배포는 지금 그대로 macOS는 `scripts/macos/install-desktop.sh` 하나입니다(일반 릴리즈 빌드 → 테스트 →
`.app`/zip → 바탕화면 설치). PGO 릴리즈는 같은 스크립트의
`--binary target/aarch64-apple-darwin/release/cheap-note`로 포장합니다.

PGO는 배포 형태로 판단합니다:

| 배포 형태 | PGO |
|---|---|
| 이 머신에서만 사용 | **적용 권장** (-7.6 %, 2 MB 감소) |
| 같은 칩 기기를 여럿 | **적용** — 같은 M6에서 프로파일 수집 |
| 불특정 다수에게 배포 | **선택** — 칩 종속이라 이득이 사라질 수 있음 |

**`RUSTFLAGS`로 플래그를 넣지 마세요.** `target-cpu=native`은 이 조합에서 손해이고, 나머지 후보는
이득이 없거나 빌드 비용만 늘립니다. 현재 `[profile.release]`가 측정상 최적점입니다.

---

## 5. 남은 병목

`rebuild`(= `build_detail` 테셀레이션) 하나뿐입니다.

- `rayon` 이미 적용 (8,182 strokes에서 23.6 → 5.0 ms, `doc/VIEW.md`)
- 현재 8,182 strokes에서 73–77 ms → **stroke당 약 9 μs**
- `present`는 GPU 시간이라 크레이트·플래그로 줄일 수 없음

컴파일 플래그로 짜낼 여지는 여기서 끝입니다. 남은 것은 **알고리즘**(재계산 범위, 캐시 적중률)이고,
먼저 `rebuild` 구간만 따로 프로파일링해야 합니다.
