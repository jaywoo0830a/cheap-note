2026년 11월 기준으로 이전 문서의 전략 구분은 유효하지만, **크레이트 버전과 유지보수 상태**에서 몇 가지 중요한 변경 사항이 확인됩니다. 아래에 전략별로 업데이트된 내용을 정리합니다.

---

## 📌 2026년 11월 기준 주요 변경 요약

| 항목 | 이전 문서 (2026년 초 기준) | **2026년 11월 기준** |
| :--- | :--- | :--- |
| **macOS 할당자** | `tikv-jemallocator = "0.6"` | **`0.7.0`** (2026-05-25 릴리스) |
| **Windows 할당자** | `snmalloc-rs = "0.3"` | **`0.7.6`** (2026-10-02), 단 **저장소 아카이브 예정** |
| **Windows API** | `windows-sys = "0.59"` | **`0.61.2`** (최신 안정) |
| **공통 mmap 크레이트** | 크로스 플랫폼 옵션으로만 언급 | **`mmap-io 1.0.0`** 안정화 (2026-05-18) |
| **신규 경량 크레이트** | — | **`oime-mmap`** (2026-11-05, 읽기 전용) |

---

## 🍎 macOS 전략: 변경 사항

### 전역 할당자: `tikv-jemallocator 0.7.0`으로 업그레이드

```toml
[target.'cfg(target_os = "macos")'.dependencies]
tikv-jemallocator = "0.7"
```

**0.7.0의 주요 변경**
- 번들된 `jemalloc`이 **5.3.0 → 5.3.1**로 업데이트됨.
- 새로운 `profiling_libunwind` 기능 추가 (프로파일링 필요 시).
- `free` FFI 관련 변경.

기존 0.6.x에서의 API 변경은 없으므로 `#[global_allocator]` 선언은 그대로 유지 가능합니다. 다만 `default` 기능에 `background_threads_runtime_support`가 포함되어 있으므로, 백그라운드 스레드 동작을 명시적으로 제어하려면 `default-features = false` 후 필요한 기능만 켜는 것을 검토할 수 있습니다.

### 메모리 매핑: `mmap-io 1.0.0`이 새로운 공통 옵션으로 부상

이전 문서에서는 macOS 전용으로 `libc` 직접 호출을 권장했지만, **`mmap-io`가 2026년 5월 18일 1.0.0 안정 버전을 출시**하면서 macOS와 Windows 모두에서 사용할 수 있는 성숙한 크로스 플랫폼 옵션이 되었습니다.

```toml
[dependencies]
mmap-io = "1.0"   # 또는 최신 1.1.0
```

**macOS 관점에서의 장점**
- `MADV_HUGEPAGE` 등 Unix 계열의 메모리 힌트를 내부적으로 활용.
- `as_slice()`로 **제로 카피** 읽기 (할당/복사 없음).
- 원자적 뷰(`atomic_u32`/`atomic_u64`)로 멀티스레드 필기 데이터 접근 시 락 없이 동시성 제어 가능.
- `1.0.0`부터 API가 SemVer로 고정되어 하위 호환성이 보장됨.

다만 `MADV_FREE_REUSABLE` 같은 **macOS 고유 플래그를 직접 제어**해야 하는 특수한 경우에는 여전히 `libc` 직접 호출이 유일한 선택지입니다. 일반적인 타일 스왑 시나리오에서는 `mmap-io`의 `advise` 기능으로 충분합니다.

---

## 🪟 Windows 전략: 변경 사항 및 주의 사항

### 전역 할당자: `snmalloc-rs`의 유지보수 상태 변화 (중요)

`snmalloc-rs`는 **0.7.6**까지 릴리스되었지만, **GitHub 저장소(`SchrodingerZhu/snmalloc-rs`)가 아카이브될 예정**이며 바인딩 개발이 **`microsoft/snmalloc` 업스트림으로 이관**되었습니다. crates.io에는 여전히 게시되고 있지만, 장기적인 유지보수 리스크를 고려해야 합니다.

```toml
[target.'cfg(windows)'.dependencies]
snmalloc-rs = "0.7"
```

**대안 검토**
- `mimalloc`: Windows에서 안정적이며 Rust 생태계에서 널리 사용됨. 다만 **스레드 로컬 해제 전략으로 인해 집중 할당 패턴에서 예상보다 큰 RAM 잔류가 발생할 수 있다**는 보고가 있습니다. 필기 앱은 짧은 수명의 할당이 많으므로 이 리스크를 인지해야 합니다.
- `snmalloc` 업스트림 바인딩이 안정화될 때까지는 **`snmalloc-rs 0.7.x`를 단기적으로 유지**하되, `Cargo.toml`에서 버전을 고정하고 업스트림 동향을 모니터링하는 전략이 현실적입니다.

### Windows API: `windows-sys 0.61.2`

```toml
windows-sys = { version = "0.61", features = [...] }
```

`0.61.0`에서 `windows-link` 크레이트를 타겟팅하도록 변경되었지만, **`CreateFileMappingW` / `MapViewOfFile` / `UnmapViewOfFile` API 자체의 시그니처는 유지**됩니다. 기존 코드에서 버전 번호만 올리면 됩니다.

### Windows 메모리 매핑: `mmap-io` 또는 `windows-sys` 직접 호출

- **`mmap-io 1.0`**: Windows에서 `PrefetchVirtualMemory`를 활용한 OS 레벨 프리페치와 `FILE_ATTRIBUTE_LARGE_PAGES` 기반 거대 페이지를 지원. Windows 전용 API를 직접 다루지 않고도 네이티브에 가까운 성능을 얻을 수 있습니다.
- **`windows-sys` 직접 호출**: `EmptyWorkingSet`이나 `VirtualAlloc(MEM_RESET)` 같은 Windows 고유의 워킹셋 제어가 필요할 때는 계속 직접 호출이 필요합니다.

---

## 💎 업데이트된 권장 조합 (2026년 11월)

| 계층 | macOS | Windows | 비고 |
| :--- | :--- | :--- | :--- |
| **메모리 매핑** | `mmap-io 1.0` (권장) 또는 `libc` 직접 | `mmap-io 1.0` 또는 `windows-sys 0.61` 직접 | `mmap-io`가 공통 옵션으로 성숙 |
| **전역 할당자** | `tikv-jemallocator 0.7` | `snmalloc-rs 0.7` (유지보수 모니터링) 또는 `mimalloc` | `snmalloc` 업스트림 이관 주시 |
| **페이지 회수** | `madvise(MADV_FREE_REUSABLE)` | `EmptyWorkingSet` / `VirtualAlloc(MEM_RESET)` | 플랫폼 고유 API 유지 |
| **제로 카피 해석** | `zerocopy` | `zerocopy` | 변경 없음 |

**핵심 판단**: 크로스 플랫폼 크레이트 생태계가 완전히 성숙했다고 보기는 어렵지만, **`mmap-io 1.0`의 안정화**로 메모리 매핑 계층에서는 공통 크레이트를 사용할 수 있는 선택지가 생겼습니다. 할당자 계층은 여전히 플랫폼별 분리가 필요하며, 특히 Windows 진영은 `snmalloc-rs`의 유지보수 이관이라는 변수를 안고 있습니다.